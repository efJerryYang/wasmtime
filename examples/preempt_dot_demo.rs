//! Preemptive scheduling demo using a 1024x1024 f64 matrix multiply loop plus
//! a sleeping companion loop. Exercises argument passing with multiple params.
//!
//! Usage:
//!   cargo run --example preempt_dot_demo           # fuel mode
//!   cargo run --example preempt_dot_demo epoch     # epoch mode

use anyhow::{ensure, Result};
use futures::executor::block_on;
use std::time::Duration;
use wasmtime::{
    Caller, Config, Engine, Linker, Memory, PreemptiveMode, Store, WasmThreadHandle,
};

struct HostState;

fn main() -> Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("epoch") => PreemptiveMode::Epoch,
        _ => PreemptiveMode::Fuel,
    };

    let mut config = Config::new();
    config.async_support(true);
    config.wasm_threads(true);
    config.wasm_stack_switching(true);
    config.wasm_preemptive_threads_mode(mode);
    match mode {
        PreemptiveMode::Fuel => {
            config.consume_fuel(true);
            config.epoch_interruption(false);
        }
        PreemptiveMode::Epoch => {
            config.consume_fuel(false);
            config.epoch_interruption(true);
        }
    }
    config.wasm_preemptive_threads(true);

    let engine = Engine::new(&config)?;
    let wat_src = include_str!("preempt_dot.wat");
    let wasm = wat::parse_str(wat_src)?;
    let module = wasmtime::Module::new(&engine, &wasm)?;

    let mut linker = Linker::new(&engine);
    linker.func_wrap("env", "sleep_ms", |_: Caller<'_, HostState>, ms: i32| {
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms as u64));
        }
    })?;
    let mut store = Store::new(&engine, HostState);
    let instance = block_on(linker.instantiate_async(&mut store, &module))?;

    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let n = 1024usize;

    // Layout: [A][B][C], each n x n f64.
    let ptr_a = 0u32;
    let bytes_mat = (n * n * 8) as u32;
    let ptr_b = ptr_a + bytes_mat;
    let ptr_c = ptr_b + bytes_mat;

    write_matrix(&mut store, &memory, ptr_a, n, |i, j| (i + j) as f64)?;
    write_matrix(&mut store, &memory, ptr_b, n, |i, j| if i == j { 1.0 } else { 0.0 })?;
    zero_region(&mut store, &memory, ptr_c, bytes_mat)?;

    let matmul_loop = instance.get_typed_func::<(i32, i32, i32, i32, i32, i32), ()>(
        &mut store,
        "matmul_loop",
    )?;

    let _matmul_thread: WasmThreadHandle = store.spawn_wasm_thread(&instance, "matmul_loop", (
        ptr_a as i32,
        ptr_b as i32,
        ptr_c as i32,
        n as i32,
        n as i32,
        n as i32,
    ))?;
    let _sleep_thread: WasmThreadHandle =
        store.spawn_wasm_thread(&instance, "sleep_loop", (5_i32,))?;

    // Keep the typed func alive for type checking.
    drop(matmul_loop);

    store.run_wasm_threads_for(Duration::from_secs(6))?;
    store.shutdown_wasm_threads()?;

    // B is identity so C should equal A.
    let samples = [(0usize, 0usize), (5usize, 7usize), (123usize, 321usize)];
    println!(
        "Inputs: A[i,j]=i+j, B=identity (n={}). Expect C == A.",
        n
    );
    for &(i, j) in &samples {
        let (expected, got, a_val) = sample_entry(&mut store, &memory, ptr_a, ptr_c, n, i, j)?;
        println!(
            "C[{i},{j}] = {got:.1} (expected {expected:.1}), A[{i},{j}] = {a_val:.1}"
        );
    }

    println!(
        "mode={:?} matmul n={} (sleep loop active) samples: {:?}",
        mode, n, samples
    );

    Ok(())
}

fn write_matrix(
    store: &mut Store<HostState>,
    memory: &Memory,
    base: u32,
    n: usize,
    mut f: impl FnMut(usize, usize) -> f64,
) -> Result<()> {
    let mut offset = base as usize;
    for i in 0..n {
        for j in 0..n {
            memory.write(&mut *store, offset, &f(i, j).to_le_bytes())?;
            offset += 8;
        }
    }
    Ok(())
}

fn zero_region(store: &mut Store<HostState>, memory: &Memory, base: u32, bytes: u32) -> Result<()> {
    let zeroes = vec![0u8; bytes as usize];
    memory.write(&mut *store, base as usize, &zeroes)?;
    Ok(())
}

fn read_entry(
    store: &mut Store<HostState>,
    memory: &Memory,
    base: u32,
    n: usize,
    i: usize,
    j: usize,
) -> Result<f64> {
    let idx = i * n + j;
    let addr = base as usize + idx * 8;
    let mut buf = [0u8; 8];
    memory.read(store, addr, &mut buf)?;
    Ok(f64::from_le_bytes(buf))
}

fn sample_entry(
    store: &mut Store<HostState>,
    memory: &Memory,
    ptr_a: u32,
    ptr_c: u32,
    n: usize,
    i: usize,
    j: usize,
) -> Result<(f64, f64, f64)> {
    let expected = (i + j) as f64;
    let got = read_entry(store, memory, ptr_c, n, i, j)?;
    let a_val = read_entry(store, memory, ptr_a, n, i, j)?;
    ensure!(
        (got - expected).abs() < 1e-6,
        "matmul mismatch at ({}, {}): got {}, expected {}",
        i,
        j,
        got,
        expected
    );
    ensure!(
        (a_val - expected).abs() < 1e-6,
        "input A corrupted at ({}, {}): got {}, expected {}",
        i,
        j,
        a_val,
        expected
    );
    Ok((expected, got, a_val))
}
