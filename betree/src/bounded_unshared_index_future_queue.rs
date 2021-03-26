//! This module provides a bounded queue of futures which are identified by a
//! key.
//!
//! This module tries to uphold the following guarantees:
//!
//! - During a flush, no further futures can be enqueued
//! - wait, wait_async, and contains_key can only indicate future completion if it has actually
//!   been completed, either via block_on or .await
//! 

use futures::{
    executor::block_on,
    future::{ IntoFuture, Shared },
    prelude::*,
};
use parking_lot::{ Mutex, RwLock };
use std::{borrow::Borrow, hash::Hash, mem, cell::RefCell, sync::Arc};
use std::sync::atomic::{ AtomicU64, Ordering };

use indexmap::IndexMap;
use dashmap::{ DashMap, DashSet };
use crossbeam_queue::SegQueue;

/// A bounded queue of `F`s which are identified by a key `K`.
pub struct BoundedFutureQueue<K, F>
where F: TryFuture, F::Output: Clone {
    // fifo: SegQueue<K>,
    // futures: DashMap<K, IntoFuture<F>>,
    // currently_draining: DashSet<K>,
    futures: RwLock<IndexMap<K, Option<IntoFuture<F>>>>,
    soft_limit: usize,
    hard_limit: usize,
    flush_lock: Mutex<()>,
    next_index: AtomicU64
}

impl<K: Clone + Eq + Hash, F: TryFuture<Ok = ()>> BoundedFutureQueue<K, F>
where F::Output: Clone, F::Error: Clone {
    /// Creates a new queue with the given `limit`.
    pub fn new(soft_limit: usize, hard_limit: usize) -> Self {
        BoundedFutureQueue {
            // fifo: SegQueue::new(),
            // futures: DashMap::new(),
            futures: RwLock::new(IndexMap::new()),
            // currently_draining: DashSet::new(),
            soft_limit, hard_limit,
            flush_lock: Mutex::new(()),
            next_index: AtomicU64::new(0)
        }
    }

    /// Enqueues a new `Future`. This function will block if the queue is full.
    pub fn enqueue(&self, key: K, future: F) -> Result<(), F::Error>
    where
        K: Clone,
    {
        {
            let _lock = self.flush_lock.lock();
            log::warn!("{}: enqueue into futures, with {} existing elements", ::std::thread::current().name().unwrap_or("none"), self.futures.read().len());
            self.wait(&key)?;
            // self.fifo.push(key.clone());
            self.futures.write().insert(key, Some(future.into_future()));
        }

        if self.futures.read().len() > self.hard_limit {
            block_on(self.drain_while_above_limit(self.hard_limit))?;
        }
        Ok(())
    }

    pub async fn mark_completed(&self, key: &K) {
        // log::warn!("{}: marked as completed", ::std::thread::current().name().unwrap_or("none"));

        // remove it from self.futures first, so nobody can grab it during draining
        self.futures.write().shift_remove(key);
        // self.currently_draining.remove(key);
        
        log::warn!("{}: marked as completed, now {} pending", ::std::thread::current().name().unwrap_or("none"), self.futures.read().len());

        if self.drain_while_above_limit(self.soft_limit).await.is_err() {
            panic!("draining failed")
        }
    }

    async fn drain_any_future(&self) -> Option<Result<(), F::Error>> {
        // Since we are competing with other workers for futures,
        // we need to retry until either we got one, or we're sure none are left.
        loop {
            // swap front to end of queue
            let next = {
                let mut queue = self.futures.write();
                let idx = self.next_index.fetch_add(1, Ordering::Relaxed) as usize % queue.len();
                log::warn!("{}: picking index {}", ::std::thread::current().name().unwrap_or("none"), idx as usize % queue.len());
                queue.get_index_mut(idx)
                    .map(|(k, v)| (k.clone(), v.take()))
            };

            match next {
                Some((key, Some(fut))) => {
                    let ret = fut.await;
                    self.futures.write().shift_remove(&key);
                    break Some(ret)
                },
                Some((_key, None)) => {
                    // another worker is handling this one, try again
                    continue
                },
                None => {
                    // no keys are left, nothing to do here
                    break None
                }
            }
        }
    }

    async fn drain_specific_future(&self, key: K) -> Option<Result<(), F::Error>> {
        /*if let Some(key) = &key {
            assert!(!self.currently_draining.contains(&key));
        }*/

        //let maybe_fut = self.futures.get_mut(&key)
        //    .and_then(|mut entry| entry.take());

        // self.currently_draining.insert(key.clone());
        let maybe_fut = self.futures.write().get_full_mut(&key)
            .map(|(_idx, k, v)| (k.clone(), v.take()));

        if let Some((key, Some(fut))) = maybe_fut {
            let ret = Some(fut.await);
            self.futures.write().shift_remove(&key);
            ret
        } else { None }
        // self.currently_draining.remove(&key);
    }

    async fn drain_while_above_limit(&self, limit: usize) -> Result<(), F::Error> {
        let now = ::std::time::Instant::now();
        let mut drained: u32 = 0;

        while self.futures.read().len() > limit { // || self.currently_draining.len() > limit {
            log::warn!("{}: going to drain one element, currently {} elements in queue", ::std::thread::current().name().unwrap_or("none"), self.futures.read().len());

            match self.drain_any_future().await {
                None => break,
                Some(res) => res?
            }

            drained += 1;
        }

        if drained != 0 {
            log::error!("{}: drained {} elements in {:?}", ::std::thread::current().name().unwrap_or("none"), drained, now.elapsed());
        }

        Ok(())
    }

    /// Flushes the queue.
    pub fn flush(&self) -> Result<(), F::Error> {
        let _lock = self.flush_lock.lock();

        let ret = block_on(self.drain_while_above_limit(0));

        /*while !self.futures.read().is_empty() { // || !self.currently_draining.is_empty() {
            log::warn!("waiting for flush, {} still pending", self.futures.read().len());
            ::std::thread::sleep_ms(10);
        }*/
        ret

        /*loop {
            match block_on(self.drain_future(None)) {
                Some(res) => { res?; },
                None => break,
            }
        }

        Ok(())*/
    }

    /// Waits asynchronously for the given `key`.
    /// May flush the whole queue if a future returned an error beforehand.
    pub fn wait_async<'a, Q: Borrow<K> + 'a>(
        &'a self,
        key: Q,
    ) -> impl TryFuture<Ok = (), Error = F::Error> + 'a {
        async move {
            self.drain_specific_future(key.borrow().clone()).await
                .unwrap_or(Ok(()))
        }
    }

    /// Waits for the given `key`.
    /// May flush the whole queue if a future returned an error beforehand.
    pub fn wait(&self, key: &K) -> Result<(), F::Error> {
        block_on(self.wait_async(key).into_future())
    }
}
