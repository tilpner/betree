use super::{
    errors::Result as StoragePoolResult, DiskOffset, StoragePoolConfiguration, StoragePoolLayer,
    TierConfiguration,
};
use crate::{
    bounded_future_queue::BoundedFutureQueue,
    buffer::Buf,
    checksum::Checksum,
    vdev::{Block, Dev, Error as VdevError, Vdev, VdevRead, VdevWrite},
};
use futures::{
    executor::{block_on, ThreadPool},
    prelude::*,
    stream::FuturesUnordered,
    task::SpawnExt,
};

use std::{convert::TryInto, io, marker::PhantomData, pin::Pin, sync::Arc};

/// Actual implementation of the `StoragePoolLayer`.
#[derive(Clone)]
pub struct StoragePoolUnit<C: Checksum> {
    inner: Arc<Inner<C>>,
}

pub(super) type WriteBackQueue = BoundedFutureQueue<
    DiskOffset,
    Pin<Box<dyn Future<Output = Result<(), VdevError>> + Send + Sync + 'static>>,
>;

const MAX_STORAGE_CLASSES: usize = 4;
type StorageTier = Box<[Dev]>;

struct Inner<C: Checksum> {
    tiers: [StorageTier; MAX_STORAGE_CLASSES],
    _check: PhantomData<Box<C>>,
    write_back_queue: WriteBackQueue,
    rt: tokio::runtime::Runtime
}

impl<C: Checksum> Inner<C> {
    fn by_offset(&self, offset: DiskOffset) -> &Dev {
        &self.tiers[offset.storage_class() as usize][offset.disk_id() as usize]
    }
}

impl<C: Checksum> StoragePoolLayer for StoragePoolUnit<C> {
    type Checksum = C;
    type Configuration = StoragePoolConfiguration;

    fn new(configuration: &Self::Configuration) -> StoragePoolResult<Self> {
        let tiers: [StorageTier; MAX_STORAGE_CLASSES] = {
            let mut vec: Vec<StorageTier> = configuration
                .tiers
                .iter()
                .map(|tier_cfg| tier_cfg.build().map(Vec::into_boxed_slice))
                .collect::<Result<Vec<_>, _>>()?;

            assert!(vec.len() <= MAX_STORAGE_CLASSES, "too many storage classes");
            vec.resize_with(MAX_STORAGE_CLASSES, || Box::new([]));
            let boxed: Box<[StorageTier; MAX_STORAGE_CLASSES]> =
                vec.into_boxed_slice().try_into().map_err(|_| ()).unwrap();
            *boxed
        };

        let devices_len = tiers.iter().map(|tier| tier.len()).sum::<usize>();
        let queue_depth = configuration.queue_depth_factor as usize * devices_len;
        Ok(StoragePoolUnit {
            inner: Arc::new(Inner {
                tiers,
                _check: PhantomData::default(),
                write_back_queue: BoundedFutureQueue::new(configuration.queue_depth_soft_limit as usize, configuration.queue_depth_hard_limit as usize),
                rt: {
                    let mut builder = tokio::runtime::Builder::new_multi_thread();
                    if let Some(threads) = configuration.thread_pool_size {
                        builder.worker_threads(threads as usize);
                    }
                    builder
                    .thread_name("storage_pool")
                    .build()?
                }
            }),
        })
    }

    type ReadAsync = Pin<Box<dyn Future<Output = Result<Buf, VdevError>> + Send>>;

    fn read_async(
        &self,
        size: Block<u32>,
        offset: DiskOffset,
        checksum: C,
    ) -> Result<Self::ReadAsync, VdevError> {
        // TODO: can move this onto pool without deadlock?
        self.inner.write_back_queue.wait(&offset)?;
        let inner = self.inner.clone();
        Ok(Box::pin(self.inner.rt.spawn(
            async move {
                // inner.write_back_queue.wait_async(offset).await;
                inner
                    .by_offset(offset)
                    .read(size, offset.block_offset(), checksum)
                    .await
            },
        ).then(|res| async {
            match res {
                Ok(Ok(res2)) => Ok(res2),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(VdevError::from(e))
            }
        })))
    }

    fn begin_write(&self, data: Buf, offset: DiskOffset) -> Result<(), VdevError> {
        let inner = self.inner.clone();

        let (enqueue_done, wait_for_enqueue) = futures::channel::oneshot::channel();
        let write = self.inner.rt.spawn(async move {
            wait_for_enqueue.await
                .unwrap();

            let res = inner
                .by_offset(offset)
                .write(data, offset.block_offset())
                .await;

            // TODO: what about multiple writes to same offset?
            /*inner.write_back_queue
                .mark_completed(&offset)
                .await;
            inner.write_back_queue
                .help_if_necessary()
                .await?;*/
            res
        }).then(|res| async {
            match res {
                Ok(Ok(res2)) => Ok(res2),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(VdevError::from(e))
            }
        });

        let ret = self.inner
            .write_back_queue
            .enqueue(offset, Box::pin(write));

        // Sending fails if receiver is dropped at this point,
        // which means the future 
        enqueue_done.send(())
            .expect("Couldn't unlock enqueued write task");

        ret
    }

    fn write_raw(&self, data: Buf, offset: Block<u64>) -> Result<(), VdevError> {
        let vec = self
            .inner
            .tiers
            .iter()
            .flat_map(|tier| tier.iter())
            .map(|vdev| vdev.write_raw(data.clone(), offset))
            .collect::<FuturesUnordered<_>>()
            .try_collect();
        block_on(vec).map(|_: Vec<()>| ())
    }

    fn read_raw(&self, size: Block<u32>, offset: Block<u64>) -> Result<Vec<Buf>, VdevError> {
        let mut vec = Vec::new();
        for class in self.inner.tiers.iter() {
            for vdev in class.iter() {
                let v = block_on(vdev.read_raw(size, offset).into_future())?;
                vec.extend(v);
            }
        }
        Ok(vec)
    }

    fn actual_size(&self, storage_class: u8, disk_id: u16, size: Block<u32>) -> Block<u32> {
        self.inner.tiers[storage_class as usize][disk_id as usize].actual_size(size)
    }

    fn size_in_blocks(&self, storage_class: u8, disk_id: u16) -> Block<u64> {
        self.inner.tiers[storage_class as usize][disk_id as usize].size()
    }

    fn num_disks(&self, storage_class: u8, disk_id: u16) -> usize {
        self.inner.tiers[storage_class as usize][disk_id as usize].num_disks()
    }

    fn effective_free_size(
        &self,
        storage_class: u8,
        disk_id: u16,
        free_size: Block<u64>,
    ) -> Block<u64> {
        self.inner.tiers[storage_class as usize][disk_id as usize].effective_free_size(free_size)
    }

    fn disk_count(&self, storage_class: u8) -> u16 {
        self.inner.tiers[storage_class as usize].len() as u16
    }

    fn storage_class_count(&self) -> u8 {
        MAX_STORAGE_CLASSES as u8
    }

    fn flush(&self) -> Result<(), VdevError> {
        self.inner.write_back_queue.flush()?;
        for tier in self.inner.tiers.iter() {
            for vdev in tier.iter() {
                vdev.flush()?;
            }
        }
        Ok(())
    }
}
