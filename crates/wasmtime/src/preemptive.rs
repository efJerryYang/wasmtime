//! Internal preemptive scheduling support for running multiple wasm exports on
//! a single host thread. This builds on the fiber-based async runtime and
//! interruption/fuel to force stack-switching without relying on a multi-
//! threaded host executor.
//!
//! This module intentionally exposes only the small surface needed by
//! `Store::spawn_wasm_thread`, `run_wasm_threads_for`, and
//! `shutdown_wasm_threads`.

#![cfg(all(feature = "runtime", feature = "async", target_has_atomic = "64"))]

use crate::StoreContextMut;
use crate::TypedFunc;
use crate::prelude::*;
use crate::runtime::fiber::{StoreFiber, StoreFiberYield, resume_fiber};
use crate::runtime::store::AsStoreOpaque;
use crate::{PreemptiveMode, UpdateDeadline};
use core::mem;
use futures::task::noop_waker;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr::NonNull;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::Context;
use std::thread;
use std::time::{Duration, Instant};
use wasmtime_fiber::set_debug_fiber_name;

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
    names: HashMap<u32, String>,
    timeslice: u64,
    mode: PreemptiveMode,
    epoch_installed: bool,
    ticker_stop: Option<Arc<AtomicBool>>,
    ticker_handle: Option<thread::JoinHandle<()>>,
    shutdown: bool,
}

impl PreemptiveThreads {
    pub(crate) fn new(timeslice: u64, mode: PreemptiveMode) -> Self {
        Self {
            timeslice,
            threads: HashMap::new(),
            run_queue: VecDeque::new(),
            enqueued: HashSet::new(),
            current: None,
            next_id: 0,
            names: HashMap::new(),
            mode,
            epoch_installed: false,
            ticker_stop: None,
            ticker_handle: None,
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

    pub(crate) fn spawn<T: Send + 'static, P: crate::WasmParams + Send + Sync + 'static>(
        &mut self,
        store: &mut crate::store::StoreInner<T>,
        func: TypedFunc<P, ()>,
        name: String,
        params: P,
    ) -> Result<WasmThreadHandle> {
        if matches!(self.mode, PreemptiveMode::Epoch) {
            self.install_epoch_callback(store);
        }
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
                let params = params;
                store_ctx.block_on(|mut cx| {
                    Box::pin(async move { func_clone.call_async(&mut cx, params).await })
                })?
            })?
        };

        self.enqueue(id);
        self.names.insert(id, name);
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
        match self.mode {
            PreemptiveMode::Fuel => {
                self.stop_epoch_ticker();
                eprintln!("[preempt][fuel] configuring slice={slice}");
                store.fuel_async_yield_interval(Some(slice))?;
                store.set_fuel(u64::MAX)?;
            }
            PreemptiveMode::Epoch => {
                self.install_epoch_callback(store);
                self.ensure_epoch_ticker(store.engine());
                eprintln!("[preempt][epoch] deadline slice={slice}");
                store.set_epoch_deadline(slice);
            }
        }

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
                let debug_name = self.names.get(&id).map(|s| s.as_str());
                set_debug_fiber_name(debug_name);
                match resume_fiber(store.as_store_opaque(), fiber, Ok(NonNull::from(cx_static))) {
                    Ok(Ok(())) => {
                        eprintln!("[preempt][run] fiber {id} completed");
                        self.finish(id);
                    }
                    Ok(Err(err)) => {
                        eprintln!("fiber {id} returned error: {err:?}");
                        eprintln!("[preempt][run] fiber {id} returned error: {err:?}");
                        self.finish(id);
                    }
                    Err(StoreFiberYield::KeepStore) | Err(StoreFiberYield::ReleaseStore) => {
                        eprintln!("[preempt][run] fiber {id} yielded");
                        self.enqueue(id);
                    }
                }
                set_debug_fiber_name(None);
            }

            self.current = None;
        }

        if matches!(self.mode, PreemptiveMode::Epoch) {
            self.stop_epoch_ticker();
        }

        Ok(())
    }

    pub(crate) fn shutdown<T>(&mut self, store: &mut crate::store::StoreInner<T>) {
        self.shutdown = true;
        for (_id, fiber) in self.threads.iter_mut() {
            fiber.dispose(store.as_store_opaque());
        }
        self.stop_epoch_ticker();
        self.threads.clear();
        self.run_queue.clear();
        self.enqueued.clear();
        self.current = None;
        self.names.clear();
    }

    fn finish(&mut self, id: u32) {
        self.enqueued.remove(&id);
        self.threads.remove(&id);
        self.names.remove(&id);
        if self.current == Some(id) {
            self.current = None;
        }
    }

    fn install_epoch_callback<T>(&mut self, store: &mut crate::store::StoreInner<T>) {
        if self.epoch_installed {
            return;
        }
        let timeslice = self.timeslice.max(1);
        store.epoch_deadline_callback(Box::new(move |ctx: StoreContextMut<'_, T>| {
            let opaque = ctx.0.as_store_opaque();
            opaque.preemptive_threads_mut().on_epoch_tick();
            Ok(UpdateDeadline::Yield(timeslice))
        }));
        self.epoch_installed = true;
    }

    pub(crate) fn on_epoch_tick(&mut self) {
        if let Some(id) = self.current.take() {
            eprintln!("[preempt][epoch] tick: requeue fiber {id}");
            self.enqueue(id);
        } else {
            eprintln!("[preempt][epoch] tick: no current fiber");
        }
    }

    fn ensure_epoch_ticker(&mut self, engine: &crate::Engine) {
        if self.ticker_handle.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let engine = engine.clone();
        // Pace epoch increments to avoid thrashing;
        let interval = Duration::from_micros(self.timeslice.max(1) * 64);
        let handle = thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                engine.increment_epoch();
                thread::sleep(interval);
            }
        });
        self.ticker_stop = Some(stop);
        self.ticker_handle = Some(handle);
    }

    fn stop_epoch_ticker(&mut self) {
        if let Some(stop) = self.ticker_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.ticker_handle.take() {
            let _ = handle.join();
        }
    }
}
