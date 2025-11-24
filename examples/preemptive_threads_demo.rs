//! Demonstrates preemptive scheduling of two infinite wasm loops on a single
//! host thread using Wasmtime's built-in stack-switching runtime.
//!
//! Run with:
//! `cargo run --example preemptive_threads_demo`

use anyhow::Result;
use futures::executor::block_on;
use std::time::Duration;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, WasmThreadHandle};

struct HostState;

fn main() -> Result<()> {
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_threads(true);
    config.wasm_stack_switching(true);
    config.epoch_interruption(true);
    config.wasm_preemptive_threads(true);

    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, "examples/preemptive_threads_demo.wat")?;

    let mut linker = Linker::new(&engine);
    linker.func_wrap("env", "log0", |_: Caller<'_, HostState>, v: i32| {
        println!("[thread0] {v}");
    })?;
    linker.func_wrap("env", "log1", |_: Caller<'_, HostState>, v: i32| {
        println!("[thread1] {v}");
    })?;

    let mut store = Store::new(&engine, HostState);
    let instance = block_on(linker.instantiate_async(&mut store, &module))?;

    let _t0: WasmThreadHandle = store.spawn_wasm_thread(&instance, "thread0", ())?;
    let _t1: WasmThreadHandle = store.spawn_wasm_thread(&instance, "thread1", ())?;

    store.run_wasm_threads_for(Duration::from_secs(1))?;
    store.shutdown_wasm_threads()?;

    Ok(())
}
