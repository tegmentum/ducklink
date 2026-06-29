//! NATIVE side of the native-vs-wasm benchmark.
//!
//! Runs ONE workload: open a bundled (native) DuckDB, register the given wasm
//! `duckdb:extension` component through the native `ducklink` extension's
//! `reg_duckdb` path, build the setup data, warm up, then execute the query
//! `--iters K` times back-to-back.
//!
//! The orchestrator (`run.py`) times this whole process EXTERNALLY at several K
//! and takes the regression slope as the marginal per-query cost; the one-time
//! open/register/component-compile/setup cost is the regression intercept and
//! cancels out. We ALSO print the internally-timed K-loop (`internal_ms`) as an
//! independent cross-check of the external slope.
//!
//! Open is done via the raw C handle (mirroring src/lib.rs) so aggregate
//! workloads can be handed a raw sibling connection (`--raw`).

use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use duckdb::ffi;
use duckdb::Connection;
use ducklink::engine::Engine2;
use ducklink::reg_duckdb::{register_components, ComponentSpec};

fn opt(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

fn run_once(con: &Connection, query: &str) -> Result<()> {
    // Re-prepare every iteration so the timed work includes parse + plan +
    // execute, matching the wasm CLI which re-parses each statement.
    let mut stmt = con.prepare(query)?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

fn main() -> Result<()> {
    let component = opt("--component").ok_or_else(|| anyhow!("--component <path> required"))?;
    let name = opt("--name").ok_or_else(|| anyhow!("--name <ext> required"))?;
    let query = opt("--query").ok_or_else(|| anyhow!("--query <sql> required"))?;
    let iters: usize = opt("--iters").unwrap_or_else(|| "20".into()).parse()?;
    let setup = opt("--setup").unwrap_or_default();
    let needs_raw = flag("--raw");

    // Open via the raw handle (mirror of native-extension/ducklink/src/lib.rs):
    // a duckdb-rs Connection for scalars/tables + an optional raw sibling for
    // aggregates (which duckdb-rs cannot register).
    let mut db: ffi::duckdb_database = ptr::null_mut();
    let mut raw_con: ffi::duckdb_connection = ptr::null_mut();
    let con: Connection;
    unsafe {
        if ffi::duckdb_open(ptr::null(), &mut db) != ffi::duckdb_state_DuckDBSuccess {
            return Err(anyhow!("duckdb_open failed"));
        }
        con = Connection::open_from_raw(db)?;
        if needs_raw
            && (ffi::duckdb_connect(db, &mut raw_con) != ffi::duckdb_state_DuckDBSuccess
                || raw_con.is_null())
        {
            return Err(anyhow!("duckdb_connect (raw sibling) failed"));
        }
    }

    let engine = Arc::new(Mutex::new(Engine2::new()?));
    let specs = vec![ComponentSpec {
        name: name.clone(),
        path: PathBuf::from(&component),
    }];
    let raw_opt = if needs_raw { Some(raw_con) } else { None };
    let registered = register_components(&con, raw_opt, engine, &specs)?;
    eprintln!("[nvw-native] registered {registered} function(s) from '{name}'");

    if !setup.trim().is_empty() {
        con.execute_batch(&setup)?;
    }

    // Warmup (full path once; excluded from the timed loop).
    run_once(&con, &query)?;

    let t = Instant::now();
    for _ in 0..iters {
        run_once(&con, &query)?;
    }
    let internal_ms = t.elapsed().as_secs_f64() * 1000.0;

    if !raw_con.is_null() {
        unsafe { ffi::duckdb_disconnect(&mut raw_con) };
    }

    // Machine-readable line for the orchestrator's cross-check.
    println!(
        "{{\"path\":\"native\",\"name\":\"{}\",\"iters\":{},\"internal_ms\":{:.6}}}",
        name, iters, internal_ms
    );
    Ok(())
}
