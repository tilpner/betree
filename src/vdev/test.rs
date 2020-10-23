use super::{
    AtomicStatistics, Block, Error, ErrorKind, ScrubResult, Statistics, Vdev, VdevLeafRead,
    VdevLeafWrite, VdevRead, VdevWrite,
};
use crate::checksum::{Builder, Checksum, State, XxHash, XxHashBuilder};
use async_trait::async_trait;
use futures::{executor::block_on, prelude::*};
use parking_lot::Mutex;
use quickcheck::{Arbitrary, Gen};
use rand::{seq::SliceRandom, Rng, RngCore, SeedableRng};
use rand_xorshift::XorShiftRng;
use seqlock::SeqLock;
use std::{collections::HashMap, sync::atomic::Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureMode {
    NoFail,
    FailOperation,
    BadData,
    Panic,
}

impl Arbitrary for FailureMode {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        *[
            FailureMode::NoFail,
            FailureMode::FailOperation,
            FailureMode::BadData,
        ]
        .choose(g)
        .unwrap()
    }
}

pub struct FailingLeafVdev {
    buffer: Mutex<Box<[u8]>>,
    id: String,
    fail_reads: SeqLock<FailureMode>,
    fail_writes: SeqLock<FailureMode>,
    fail_flushes: SeqLock<bool>,
    stats: AtomicStatistics,
}

impl FailingLeafVdev {
    pub fn new(size: Block<u32>, id: String) -> Self {
        FailingLeafVdev {
            buffer: Mutex::new(vec![0; size.to_bytes() as usize].into_boxed_slice()),
            id,
            fail_reads: SeqLock::new(FailureMode::NoFail),
            fail_writes: SeqLock::new(FailureMode::NoFail),
            fail_flushes: SeqLock::new(false),
            stats: Default::default(),
        }
    }

    pub fn fail_writes(&self, failure_mode: FailureMode) {
        *self.fail_writes.lock_write() = failure_mode;
    }

    pub fn fail_reads(&self, failure_mode: FailureMode) {
        *self.fail_reads.lock_write() = failure_mode;
    }
}

impl FailingLeafVdev {
    fn handle_read(&self, size: Block<u32>, offset: Block<u64>) -> Result<Box<[u8]>, Error> {
        let size = size.to_bytes() as usize;
        let offset = offset.to_bytes() as usize;
        let end_offset = offset + size;

        match self.fail_reads.read() {
            FailureMode::NoFail => {
                let b = self.buffer.lock()[offset..end_offset]
                    .to_vec()
                    .into_boxed_slice();
                Ok(b)
            }
            FailureMode::FailOperation | FailureMode::BadData => {
                Err(ErrorKind::ReadError(self.id.clone()).into())
            }
            FailureMode::Panic => panic!(),
        }
    }
}

#[async_trait]
impl<C: Checksum> VdevRead<C> for FailingLeafVdev {
    async fn read(
        &self,
        size: Block<u32>,
        offset: Block<u64>,
        checksum: C,
    ) -> Result<Box<[u8]>, Error> {
        let b = self.handle_read(size, offset)?;
        match checksum.verify(&b) {
            Ok(()) => Ok(b),
            Err(_) => Err(ErrorKind::ReadError(self.id.clone()).into()),
        }
    }

    async fn scrub(
        &self,
        size: Block<u32>,
        offset: Block<u64>,
        checksum: C,
    ) -> Result<ScrubResult, Error> {
        let data = self.read(size, offset, checksum).await?;
        Ok(ScrubResult {
            data,
            faulted: Block(0),
            repaired: Block(0),
        })
    }

    async fn read_raw(
        &self,
        size: Block<u32>,
        offset: Block<u64>,
    ) -> Result<Vec<Box<[u8]>>, Error> {
        let b = self.handle_read(size, offset)?;
        Ok(vec![b])
    }
}

impl Vdev for FailingLeafVdev {
    fn actual_size(&self, size: Block<u32>) -> Block<u32> {
        size
    }

    fn size(&self) -> Block<u64> {
        Block::from_bytes(self.buffer.lock().len() as u64)
    }

    fn effective_free_size(&self, free_size: Block<u64>) -> Block<u64> {
        free_size
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn stats(&self) -> Statistics {
        self.stats.as_stats()
    }

    fn for_each_child(&self, _f: &mut dyn FnMut(&dyn Vdev)) {}

    fn num_disks(&self) -> usize {
        1
    }
}

#[async_trait]
impl<T: AsMut<[u8]> + Send + 'static> VdevLeafRead<T> for FailingLeafVdev {
    async fn read_raw(&self, mut buf: T, offset: Block<u64>) -> Result<T, Error> {
        let size = Block::from_bytes(buf.as_mut().len() as u32);
        self.stats.read.fetch_add(size.as_u64(), Ordering::Relaxed);

        let offset = offset.to_bytes() as usize;
        let byte_size = size.to_bytes() as usize;
        let end_offset = offset + byte_size;
        assert!(
            end_offset <= self.buffer.lock().len(),
            format!("{} <= {}", end_offset, self.buffer.lock().len())
        );

        let v = match self.fail_reads.read() {
            FailureMode::NoFail => self.buffer.lock()[offset..end_offset].to_vec(),
            FailureMode::FailOperation => {
                self.stats
                    .failed_reads
                    .fetch_add(size.as_u64(), Ordering::Relaxed);
                return Err(Error::from(ErrorKind::ReadError(self.id.clone())));
            }
            FailureMode::BadData => (0..byte_size)
                .map(|x| (3 * x + offset) as u8)
                .collect::<Vec<_>>(),
            FailureMode::Panic => panic!(),
        };
        buf.as_mut().copy_from_slice(&v);
        Ok(buf)
    }

    fn checksum_error_occurred(&self, size: Block<u32>) {
        self.stats
            .checksum_errors
            .fetch_add(size.as_u64(), Ordering::Relaxed);
    }
}

#[async_trait]
impl VdevLeafWrite for FailingLeafVdev {
    async fn write_raw<T: AsRef<[u8]> + Send>(
        &self,
        data: T,
        offset: Block<u64>,
        is_repair: bool,
    ) -> Result<(), Error> {
        let size_in_blocks = Block::from_bytes(data.as_ref().len() as u64).as_u64();
        self.stats
            .written
            .fetch_add(size_in_blocks, Ordering::Relaxed);

        let offset = offset.to_bytes() as usize;
        let end_offset = offset + data.as_ref().len();
        let bad_data;
        let slice = match self.fail_writes.read() {
            FailureMode::NoFail => &data.as_ref()[..],
            FailureMode::FailOperation => {
                self.stats
                    .failed_writes
                    .fetch_add(size_in_blocks, Ordering::Relaxed);
                return Err(Error::from(ErrorKind::WriteError(self.id.clone())));
            }
            FailureMode::BadData => {
                bad_data = (0..data.as_ref().len())
                    .map(|x| (7 * x + offset) as u8)
                    .collect::<Vec<_>>();
                &bad_data[..]
            }
            FailureMode::Panic => panic!(),
        };
        self.buffer.lock()[offset..end_offset].copy_from_slice(slice);
        if is_repair {
            self.stats
                .repaired
                .fetch_add(size_in_blocks, Ordering::Relaxed);
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), Error> {
        if self.fail_flushes.read() {
            Err(Error::from(ErrorKind::WriteError(self.id.clone())))
        } else {
            Ok(())
        }
    }
}

#[macro_export]
macro_rules! try_ret {
    ($expr:expr) => {
        match $expr {
            ::std::result::Result::Ok(val) => val,
            ::std::result::Result::Err(err) => return err,
        }
    };
}

pub fn generate_data(idx: usize, offset: Block<u64>, size: Block<u32>) -> Box<[u8]> {
    let seed = [
        size.as_u32() + 1,
        idx as u32,
        offset.as_u64() as u32,
        (offset.as_u64() >> 32) as u32,
    ];
    let mut rng = XorShiftRng::from_seed(unsafe { ::std::mem::transmute(seed) });

    let mut data = vec![0; size.to_bytes() as usize].into_boxed_slice();
    rng.fill_bytes(&mut data);
    data
}

#[derive(Debug, Clone, Copy)]
pub struct NonZeroU8(u8);

impl Arbitrary for NonZeroU8 {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        NonZeroU8(g.gen_range(1, 255))
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        Box::new(self.0.shrink().filter(|&x| x != 0).map(NonZeroU8))
    }
}

pub fn test_writes_are_persistent<V: Vdev + VdevRead<XxHash> + VdevWrite>(
    writes: &[(u8, u8)],
    vdev: &V,
) {
    let mut marker = vec![0; vdev.size().as_u64() as usize];
    let mut checksums = HashMap::new();
    for (idx, &(offset, size)) in writes.iter().enumerate() {
        let offset = Block(offset as u64);
        let size = Block(size as u32);
        let actual_size = vdev.actual_size(size);
        assert!(offset + actual_size.as_u64() <= vdev.size());
        for m in marker[offset.as_u64() as usize..(offset.as_u64() + actual_size.0 as u64) as usize]
            .iter_mut()
        {
            *m = idx;
        }
        let data = generate_data(idx, offset, size);
        let checksum = {
            let mut state = XxHashBuilder.build();
            state.ingest(&data);
            state.finish()
        };
        checksums.insert(idx, checksum);

        block_on(vdev.write(data.clone(), offset).into_future()).expect("Write failed");
    }
    for (idx, &(offset, size)) in writes.iter().enumerate() {
        let size = Block(size as u32);
        let actual_size = vdev.actual_size(size);
        if marker[offset as usize..(offset as u32 + actual_size.0) as usize]
            .iter()
            .any(|&x| x != idx)
        {
            continue;
        }
        let offset = Block(offset as u64);
        let checksum = checksums[&idx];
        let data = block_on(vdev.read(size, offset, checksum).into_future()).expect("Read failed");
        let gen_data = generate_data(idx, offset, size);
        assert!(checksum.verify(&data).is_ok());
        assert!(checksum.verify(&gen_data).is_ok());
        assert_eq!(data, gen_data);
    }
}
