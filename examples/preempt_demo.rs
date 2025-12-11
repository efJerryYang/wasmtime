//! Demonstrates preemptive scheduling of three wasm loops on a single host
//! thread, each doing a small host-side sleep every iteration to slow things
//! down and make interleaving obvious.
//!
//! Run with:
//! `cargo run --example preemptive_threads_demo_slow`

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
    // Preemption is driven by fuel; avoid epoch traps.
    config.epoch_interruption(false);
    config.consume_fuel(true);
    config.wasm_preemptive_threads(true);

    let engine = Engine::new(&config)?;
    let wat_src = include_str!("preempt_demo.wat");
    let wasm = wat::parse_str(wat_src)?;
    let module = Module::new(&engine, &wasm)?;

    let mut linker = Linker::new(&engine);
    linker.func_wrap("env", "log0", |_: Caller<'_, HostState>, v: i32| {
        println!("[thread0] {v}");
    })?;
    linker.func_wrap("env", "log1", |_: Caller<'_, HostState>, v: i32| {
        println!("[thread1] {v}");
    })?;
    linker.func_wrap("env", "log2", |_: Caller<'_, HostState>, v: i32| {
        println!("[thread2] {v}");
    })?;
    linker.func_wrap("env", "sleep_ms", |_: Caller<'_, HostState>, ms: i32| {
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms as u64));
        }
    })?;

    let mut store = Store::new(&engine, HostState);
    let instance = block_on(linker.instantiate_async(&mut store, &module))?;

    let _t0: WasmThreadHandle = store.spawn_wasm_thread(&instance, "thread0", ())?;
    let _t1: WasmThreadHandle = store.spawn_wasm_thread(&instance, "thread1", ())?;
    let _t2: WasmThreadHandle = store.spawn_wasm_thread(&instance, "thread2", ())?;

    store.run_wasm_threads_for(Duration::from_secs(10))?;
    store.shutdown_wasm_threads()?;

    Ok(())
}
