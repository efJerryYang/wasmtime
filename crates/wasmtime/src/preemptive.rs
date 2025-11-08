//! Internal preemptive scheduling support for running multiple wasm exports on
//! a single host thread. This builds on the fiber-based async runtime and
//! epoch-based interruption to force stack-switching without relying on a
//! multi-threaded host executor.
//!
//! This module intentionally exposes only the small surface needed by
//! `Store::spawn_wasm_thread`, `run_wasm_threads_for`, and
//! `shutdown_wasm_threads`.

#![cfg(all(feature = "runtime", feature = "async", target_has_atomic = "64"))]

use crate::prelude::*;
use crate::runtime::fiber::{resume_fiber, StoreFiber, StoreFiberYield};
use crate::runtime::store::AsStoreOpaque;
use crate::{Engine, StoreContextMut, TypedFunc, UpdateDeadline};
use core::mem;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr::NonNull;
use std::task::Context;
use std::time::{Duration, Instant};
use std::thread;
use futures::task::noop_waker;

/// Opaque handle returned to the embedder for spawned wasm threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WasmThreadHandle {
    pub(crate) id: u32,
}

#[derive(Default)]
pub(crate) struct PreemptiveThreads {
    threads: HashMap<u32, StoreFiber<'static>>,
    run_queue: VecDeque<u32>,
    enqueued: HashSet<u32>,
    current: Option<u32>,
    next_id: u32,
    timeslice: u64,
    epoch_installed: bool,
    shutdown: bool,
    ticker_stop: Option<Arc<AtomicBool>>,
    ticker_handle: Option<thread::JoinHandle<()>>,
}

impl PreemptiveThreads {
    pub(crate) fn new(timeslice: u64) -> Self {
        Self {
            timeslice,
            ..Default::default()
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    fn enqueue(&mut self, id: u32) {
        if self.enqueued.insert(id) {
            self.run_queue.push_back(id);
        }
    }

    pub(crate) fn on_epoch_tick(&mut self) {
        if let Some(id) = self.current.take() {
            eprintln!("[preempt][epoch] tick: requeue current fiber {id}");
            self.enqueue(id);
        } else {
            eprintln!("[preempt][epoch] tick: no current fiber");
        }
    }

    fn pop_next(&mut self) -> Option<u32> {
        while let Some(id) = self.run_queue.pop_front() {
            if self.threads.contains_key(&id) {
                self.enqueued.remove(&id);
                self.current = Some(id);
                return Some(id);
            }
        }
        None
    }

    pub(crate) fn install_epoch_callback<T>(&mut self, store: &mut crate::store::StoreInner<T>) {
        if self.epoch_installed {
            return;
        }
        let timeslice = self.timeslice;
        store.set_epoch_deadline(timeslice);
        store.epoch_deadline_callback(Box::new(move |ctx: StoreContextMut<'_, T>| {
            let opaque = ctx.0.as_store_opaque();
            opaque.preemptive_threads_mut().on_epoch_tick();
            Ok(UpdateDeadline::Yield(timeslice))
        }));
        self.epoch_installed = true;

        if self.ticker_handle.is_none() {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_clone = stop.clone();
            let engine = store.engine().clone();
            let interval = Duration::from_millis(self.timeslice.max(1));
            let handle = thread::spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    engine.increment_epoch();
                    thread::sleep(interval);
                }
            });
            self.ticker_stop = Some(stop);
            self.ticker_handle = Some(handle);
        }
    }

    pub(crate) fn spawn<T: Send + 'static>(
        &mut self,
        store: &mut crate::store::StoreInner<T>,
        func: TypedFunc<(), ()>,
    ) -> Result<WasmThreadHandle> {
        self.install_epoch_callback(store);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        eprintln!("[preempt][spawn] fiber {id} for export");

        // Run the wasm export on its own fiber; epoch-driven yields will
        // suspend the fiber with `StoreFiberYield::ReleaseStore` when
        // `wasm_preemptive_threads` is enabled.
        let func_clone = func.clone();
        let fiber = unsafe {
            crate::runtime::fiber::make_fiber_unchecked(store, move |store| {
                let store_ctx = StoreContextMut(store);
                store_ctx.block_on(|mut cx| Box::pin(async move { func_clone.call_async(&mut cx, ()).await }))?
            })?
        };

        self.enqueue(id);
        self.threads.insert(id, fiber);
        Ok(WasmThreadHandle { id })
    }

    pub(crate) fn run_for<T>(
        &mut self,
        store: &mut crate::store::StoreInner<T>,
        engine: &Engine,
        duration: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        while !self.shutdown && start.elapsed() < duration {
            eprintln!("[preempt][run] loop tick");
            engine.increment_epoch();

            let Some(id) = self.pop_next() else {
                eprintln!("[preempt][run] run queue empty");
                break;
            };

            eprintln!("[preempt][run] schedule fiber {id}");
            {
                let fiber = self.threads.get_mut(&id).unwrap();
                let cx_static: &mut Context<'static> = unsafe { mem::transmute(&mut cx) };
                match resume_fiber(
                    store.as_store_opaque(),
                    fiber,
                    Ok(NonNull::from(cx_static)),
                ) {
                    Ok(Ok(())) => {
                        eprintln!("[preempt][run] fiber {id} completed");
                        self.finish(id);
                    }
                    Ok(Err(err)) => {
                        log::trace!("fiber {id} returned error: {err}");
                        eprintln!("[preempt][run] fiber {id} returned error: {err}");
                        self.finish(id);
                    }
                    Err(StoreFiberYield::KeepStore) | Err(StoreFiberYield::ReleaseStore) => {
                        eprintln!("[preempt][run] fiber {id} yielded");
                        self.enqueue(id);
                    }
                }
            }

            self.current = None;
        }

        Ok(())
    }

    pub(crate) fn shutdown<T>(&mut self, store: &mut crate::store::StoreInner<T>) {
        self.shutdown = true;
        for (_id, fiber) in self.threads.iter_mut() {
            fiber.dispose(store.as_store_opaque());
        }
        self.threads.clear();
        self.run_queue.clear();
        self.enqueued.clear();
        self.current = None;
        if let Some(stop) = self.ticker_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.ticker_handle.take() {
            let _ = handle.join();
        }
    }

    fn finish(&mut self, id: u32) {
        self.enqueued.remove(&id);
        self.threads.remove(&id);
        if self.current == Some(id) {
            self.current = None;
        }
    }
}
