//! This module provides a bounded queue of futures which are identified by a
//! key.

use futures::{
    executor::block_on,
    future::IntoFuture,
    prelude::*,
};
use parking_lot::ReentrantMutex;
use parking_lot::{ Mutex, Condvar };
use std::{borrow::Borrow, hash::Hash, mem, cell::RefCell, sync::Arc, time::Instant, thread};

use indexmap::IndexMap;
use dashmap::DashMap;

/// A bounded queue of `F`s which are identified by a key `K`.
pub struct BoundedFutureQueue<K, F> {
    futures: DashMap<K, IntoFuture<F>>,
    soft_limit: u64,
    hard_limit: u64,
    
    flush_lock: Mutex<()>,
    tracking: Arc<(Mutex<u64>, Condvar)>,
}

impl<K: Clone + Eq + Hash, F: TryFuture<Ok = ()>> BoundedFutureQueue<K, F> {
    /// Creates a new queue with the given `limit`.
    pub fn new(soft_limit: usize, hard_limit: usize) -> Self {
        BoundedFutureQueue {
            futures: DashMap::new(),
            soft_limit: soft_limit as u64,
            hard_limit: hard_limit as u64,
            flush_lock: Mutex::new(()),
            tracking: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    /// Enqueues a new `Future`. This function will block if the queue is full.
    pub fn enqueue(&self, key: K, future: F) -> Result<(), F::Error>
    where
        K: Clone,
    {
        let _flush = self.flush_lock.lock();

        log::trace!("{}: enqueue into futures, with {} existing elements", thread::current().name().unwrap_or("none"), self.futures.len());
        self.wait(&key)?;
        
        let &(ref pending, ref cvar) = &*self.tracking;
        loop {
            let mut currently_pending = pending.lock();
            log::trace!("enqueue, self.futures.len() = {}, currently_pending = {}, hard_limit = {}",
                self.futures.len(), *currently_pending, self.hard_limit);

            if *currently_pending <= self.hard_limit {
                *currently_pending = currently_pending.checked_add(1).expect("overflow");
                self.futures.insert(key, future.into_future());
                break
            } else {
                let now = Instant::now();
                cvar.wait(&mut currently_pending);
                log::info!("enqueue: too full, waited for {:?}", now.elapsed());
            }
        }

        Ok(())
    }


    fn remove_if_present(&self, k: &K) {
        // avoid "double-free", another worker might have concurrently completed
        // the work associated with k

        let _ = self.futures.remove(k);

        let &(ref pending, ref cvar) = &*self.tracking;
        {
            let mut pending = pending.lock();
            *pending = pending.checked_sub(1).expect("underflow");
            cvar.notify_all();
        }


        /*
        if let Some(_) = self.futures.remove(k) {
            let &(ref pending, ref cvar) = &*self.tracking;
            {
                let mut pending = pending.lock();
                *pending = pending.checked_sub(1).expect("underflow");
            }
            cvar.notify_all();
        } else {
            log::error!("tried to mark future as complete, but wasn't tracked!");
        }*/
    }


    /// Returns false if the queue does not contain `key`.
    pub fn contains_key(&self, key: &K) -> bool {
        // RefCell::borrow(&self.futures.lock()).contains_key(key)
        self.futures.contains_key(key)
    }

    /// Mark a task as completed. The intention is, that by removing finished tasks
    /// as soon as possible, the queue will contain mostly actual work.
    /// If it also contained a lot of finished tasks, enqueue would have to apply
    /// back-pressure unnecessarily.
    pub async fn mark_completed(&self, key: &K) {
        log::trace!("{}: marked as completed", ::std::thread::current().name().unwrap_or("none"));
        // self.futures.lock().borrow_mut().shift_remove(key);
        
        // The key may already have been removed by someone else
        self.remove_if_present(key);
    }

    pub async fn help_if_necessary(&self) -> Result<(), F::Error> {
        self.drain_while_above_limit(self.soft_limit).await
    }

    fn remove_arbitrary_future(&self) -> Option<IntoFuture<F>> {
        // Need to be careful here to avoid DashMap deadlock, can't hold
        // any references into self.futures before calling remove

        loop {
            let maybe_key = self.futures.iter().next()
                .map(|entry| K::clone(entry.key()));

            if let Some(key) = maybe_key {
                match self.futures.remove(&key) {
                    Some((_k, v)) => break Some(v),
                    // We got a key, but no futures was registered for it yet,
                    //
                    // Some((_k, None)) => continue,
                    // If we got a key but there was no future (anymore),
                    // another worker stole it from us, so try again
                    None => continue

                }
            } else {
                // If we didn't get even a key, the map is empty and
                // we're done here
                break None
            }
        }
    }

    /// Waits asynchronously for the given `key`.
    /// May flush the whole queue if a future returned an error beforehand.
    pub fn wait_async<'a, Q: Borrow<K> + 'a>(
        &'a self,
        key: Q,
    ) -> impl TryFuture<Ok = (), Error = F::Error> + 'a {
        let maybe_fut = self.futures.remove(key.borrow());

        let tracking = self.tracking.clone();
        async move {
            match maybe_fut {
                Some((_k, fut)) => {
                    let ret = fut.await;
                    let &(ref pending, ref cvar) = &*tracking;
                    {
                        let mut pending = pending.lock();
                        *pending = pending.checked_sub(1).expect("underflow");
                        cvar.notify_all();
                    }
                    ret
                }
                /*Some((_k, None)) => {
                    // Work for this key was announced but not enqueued yet
                    panic!("waiting for announced, but not enqueued work");
                },*/
                None => Ok(())
            }
        }
    }

    /// Waits for the given `key`.
    /// May flush the whole queue if a future returned an error beforehand.
    pub fn wait(&self, key: &K) -> Result<(), F::Error> {
        block_on(self.wait_async(key).into_future())
    }

    /// Flushes the queue.
    pub fn flush(&self) -> Result<(), F::Error> {
        let _flush = self.flush_lock.lock();

        // block_on(self.drain_while_above_limit(0))?;

        // No more futures stored, wait for currently draining workers
        let &(ref pending, ref cvar) = &*self.tracking;
        loop {
            let mut still_pending = pending.lock();
            if *still_pending == 0 {
                break;
            } else {
                log::info!("flush: waiting on {} in-flight futures", *still_pending);
                cvar.wait(&mut still_pending);
            }
        }

        Ok(())
    }

    async fn drain_while_above_limit(&self, limit: u64) -> Result<(), F::Error> {
        // let len = RefCell::borrow(&self.futures.lock()).len();
        let now = Instant::now();
        let mut drained = 0;

        while self.futures.len() as u64 > limit {
            log::warn!("{}: going to drain one element, currently {} elements in queue", thread::current().name().unwrap_or("none"), self.futures.len());

            if let Some(fut) = self.remove_arbitrary_future() {
                drained += 1;

                let &(ref pending, ref cvar) = &*self.tracking;
                let ret = fut.await;
                {
                    let mut pending = pending.lock();
                    *pending = pending.checked_sub(1).expect("underflow");
                    cvar.notify_all();
                }
                ret?;
            } else { break }
        }

        if drained != 0 {
            log::warn!("{}: drained {} elements in {:?}", thread::current().name().unwrap_or("none"), drained, now.elapsed());
        }

        Ok(())
    }
}
