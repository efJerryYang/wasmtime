//! Internal preemptive scheduling support for running multiple wasm exports on
//! a single host thread. This builds on the fiber-based async runtime and
//! interruption/fuel to force stack-switching without relying on a multi-
//! threaded host executor.
//!
//! This module intentionally exposes only the small surface needed by
//! `Store::spawn_wasm_thread`, `run_wasm_threads_for`, and
//! `shutdown_wasm_threads`.

#![cfg(all(feature = "runtime", feature = "async", target_has_atomic = "64"))]

use crate::prelude::*;
use crate::runtime::fiber::{resume_fiber, StoreFiber, StoreFiberYield};
use crate::runtime::store::AsStoreOpaque;
use crate::StoreContextMut;
use crate::TypedFunc;
use core::mem;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr::NonNull;
use std::task::Context;
use std::time::{Duration, Instant};
use futures::task::noop_waker;

/// Opaque handle returned to the embedder for spawned wasm threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WasmThreadHandle {
    pub(crate) id: u32,
}

pub(crate) struct PreemptiveThreads {
    threads: HashMap<u32, StoreFiber<'static>>,
    run_queue: VecDeque<u32>,
    enqueued: HashSet<u32>,
    current: Option<u32>,
    next_id: u32,
    timeslice: u64,
    shutdown: bool,
}

impl PreemptiveThreads {
    pub(crate) fn new(timeslice: u64) -> Self {
        Self {
            timeslice,
            threads: HashMap::new(),
            run_queue: VecDeque::new(),
            enqueued: HashSet::new(),
            current: None,
            next_id: 0,
            shutdown: false,
        }
    }

    fn enqueue(&mut self, id: u32) {
        if self.enqueued.insert(id) {
            self.run_queue.push_back(id);
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

    pub(crate) fn spawn<T: Send + 'static>(
        &mut self,
        store: &mut crate::store::StoreInner<T>,
        func: TypedFunc<(), ()>,
    ) -> Result<WasmThreadHandle> {
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
                store_ctx.block_on(|mut cx| {
                    Box::pin(async move { func_clone.call_async(&mut cx, ()).await })
                })?
            })?
        };

        self.enqueue(id);
        self.threads.insert(id, fiber);
        Ok(WasmThreadHandle { id })
    }

    pub(crate) fn run_for<T>(
        &mut self,
        store: &mut crate::store::StoreInner<T>,
        duration: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let slice = self.timeslice.max(1);
        eprintln!("[preempt][fuel] configuring slice={slice}");
        store.fuel_async_yield_interval(Some(slice))?;
        store.set_fuel(u64::MAX)?;

        while !self.shutdown && start.elapsed() < duration {
            eprintln!("[preempt][run] loop tick");

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
                        log::trace!("fiber {id} returned error: {err:?}");
                        eprintln!("[preempt][run] fiber {id} returned error: {err:?}");
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
    }

    fn finish(&mut self, id: u32) {
        self.enqueued.remove(&id);
        self.threads.remove(&id);
        if self.current == Some(id) {
            self.current = None;
        }
    }
}
