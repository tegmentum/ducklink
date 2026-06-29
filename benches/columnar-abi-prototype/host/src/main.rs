use std::time::Instant;

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../guest/wit/world.wit",
    world: "dispatch",
});

use bench::dispatch::types::{Colvec, Column, Duckvalue};

struct Ctx {
    table: ResourceTable,
    wasi: WasiCtx,
}
impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

const TOTAL: usize = 1_000_000;
const CHUNK: usize = 2048;

fn main() -> anyhow::Result<()> {
    let wasm = "../guest/target/wasm32-wasip1/release/colbench_guest.wasm";
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, wasm)?;

    let mut linker: Linker<Ctx> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let mut store = Store::new(
        &engine,
        Ctx {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().build(),
        },
    );

    let inst = Dispatch::instantiate(&mut store, &component, &linker)?;
    let rowmajor = inst.bench_dispatch_rowmajor();
    let columnar = inst.bench_dispatch_columnar();

    // number of full chunks
    let chunks = TOTAL / CHUNK;
    let iters = 20u32; // repeat the whole 1M pass

    // ---- warmup ----
    {
        let rows: Vec<Vec<Duckvalue>> =
            (0..CHUNK as i64).map(|i| vec![Duckvalue::Int64(i)]).collect();
        let _ = rowmajor.call_call_scalar_batch(&mut store, &rows)?;
        let col = Colvec {
            data: Column::Int64((0..CHUNK as i64).collect()),
            validity: vec![],
            rows: CHUNK as u32,
        };
        let _ = columnar.call_call_scalar_batch_col(&mut store, &[col])?;
    }

    // ---- ROW-MAJOR: build Vec<Vec<Duckvalue>> per chunk + cross ----
    let mut row_acc: i64 = 0;
    let start = Instant::now();
    for _ in 0..iters {
        for c in 0..chunks {
            let base = (c * CHUNK) as i64;
            let rows: Vec<Vec<Duckvalue>> = (0..CHUNK as i64)
                .map(|i| vec![Duckvalue::Int64(base + i)])
                .collect();
            let out = rowmajor.call_call_scalar_batch(&mut store, &rows)?.unwrap();
            if let Some(Duckvalue::Int64(n)) = out.first() {
                row_acc = row_acc.wrapping_add(*n);
            }
        }
    }
    let row_elapsed = start.elapsed();

    // ---- COLUMNAR: build typed column per chunk + cross ----
    let mut col_acc: i64 = 0;
    let start = Instant::now();
    for _ in 0..iters {
        for c in 0..chunks {
            let base = (c * CHUNK) as i64;
            let data: Vec<i64> = (0..CHUNK as i64).map(|i| base + i).collect();
            let col = Colvec {
                data: Column::Int64(data),
                validity: vec![],
                rows: CHUNK as u32,
            };
            let out = columnar
                .call_call_scalar_batch_col(&mut store, &[col])?
                .unwrap();
            if let Column::Int64(v) = out.data {
                if let Some(n) = v.first() {
                    col_acc = col_acc.wrapping_add(*n);
                }
            }
        }
    }
    let col_elapsed = start.elapsed();

    // ---- BOUNDARY-ONLY: prebuild ALL inputs once, time only the crossings ----
    let row_inputs: Vec<Vec<Vec<Duckvalue>>> = (0..chunks)
        .map(|c| {
            let base = (c * CHUNK) as i64;
            (0..CHUNK as i64)
                .map(|i| vec![Duckvalue::Int64(base + i)])
                .collect()
        })
        .collect();
    let start = Instant::now();
    let mut row_b: i64 = 0;
    for _ in 0..iters {
        for rows in &row_inputs {
            let out = rowmajor.call_call_scalar_batch(&mut store, rows)?.unwrap();
            if let Some(Duckvalue::Int64(n)) = out.first() {
                row_b = row_b.wrapping_add(*n);
            }
        }
    }
    let row_b_elapsed = start.elapsed();

    let col_inputs: Vec<Vec<i64>> = (0..chunks)
        .map(|c| {
            let base = (c * CHUNK) as i64;
            (0..CHUNK as i64).map(|i| base + i).collect()
        })
        .collect();
    let start = Instant::now();
    let mut col_b: i64 = 0;
    for _ in 0..iters {
        for data in &col_inputs {
            let col = Colvec {
                data: Column::Int64(data.clone()),
                validity: vec![],
                rows: CHUNK as u32,
            };
            let out = columnar.call_call_scalar_batch_col(&mut store, &[col])?.unwrap();
            if let Column::Int64(v) = out.data {
                if let Some(n) = v.first() {
                    col_b = col_b.wrapping_add(*n);
                }
            }
        }
    }
    let col_b_elapsed = start.elapsed();

    let total_rows = (iters as usize * chunks * CHUNK) as f64;
    let row_ns = row_elapsed.as_nanos() as f64 / total_rows;
    let col_ns = col_elapsed.as_nanos() as f64 / total_rows;

    println!("rows processed each: {:.0} ({} iters x {} chunks x {} rows)", total_rows, iters, chunks, CHUNK);
    println!("checksums: row={} col={}", row_acc, col_acc);
    println!();
    println!("ROW-MAJOR  list<list<duckvalue>>  : {:>8.2} ms total   {:>7.2} ns/row", row_elapsed.as_secs_f64() * 1e3, row_ns);
    println!("COLUMNAR   list<colvec>           : {:>8.2} ms total   {:>7.2} ns/row", col_elapsed.as_secs_f64() * 1e3, col_ns);
    println!();
    println!("speedup: {:.2}x   latency reduction: {:.1}%", row_ns / col_ns, (1.0 - col_ns / row_ns) * 100.0);

    let row_b_ns = row_b_elapsed.as_nanos() as f64 / total_rows;
    let col_b_ns = col_b_elapsed.as_nanos() as f64 / total_rows;
    println!();
    println!("--- boundary-only (inputs prebuilt; col still clones buffer/chunk) ---");
    println!("ROW-MAJOR  : {:>7.2} ns/row   (checksum {})", row_b_ns, row_b);
    println!("COLUMNAR   : {:>7.2} ns/row   (checksum {})", col_b_ns, col_b);
    println!("speedup: {:.2}x   latency reduction: {:.1}%", row_b_ns / col_b_ns, (1.0 - col_b_ns / row_b_ns) * 100.0);
    Ok(())
}
