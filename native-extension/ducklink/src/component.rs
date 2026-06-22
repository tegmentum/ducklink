//! Component loading + the `ducklink_load` control function.
//!
//! This module owns the Direction-2 sink: it converts the neutral
//! `ducklink_runtime::reg::*` records captured from a component's `load()` into
//! DuckDB C-API function registrations, and routes per-row invocations back into
//! wasmtime through the shared `CallbackRegistry`.

use std::error::Error;

use duckdb::Connection;

/// Register the `ducklink_load(path VARCHAR)` control function on `con`.
///
/// `ducklink_load` instantiates the component at `path`, runs its `load()` to
/// capture `reg::*` registrations, then registers each as a DuckDB function.
pub fn register_loader(_con: &Connection) -> Result<(), Box<dyn Error>> {
    // TODO: register a DuckDB table/pragma function "ducklink_load" whose body:
    //   1. let engine = ducklink_runtime engine (wasmtime Engine, component-model)
    //   2. let component = Component::from_file(&engine, path)?
    //   3. instantiate against ducklink_runtime::duckdb_extension_bindings with a
    //      store-state implementing the Host* capability traits (shared from
    //      ducklink-runtime once extracted), call `load()`
    //   4. for each captured reg::ScalarReg, register_scalar with a per-row
    //      callback that re-enters the component via CallbackRegistry.
    //
    // Blocked on the capture store-state extraction (remaining Phase C). Tracked
    // in the crate docs and project memory.
    Ok(())
}
