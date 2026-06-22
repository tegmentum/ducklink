//! ducklink — a native-DuckDB loadable extension that embeds wasmtime to run
//! `duckdb:extension` WebAssembly components (Direction 2).
//!
//! # Architecture
//!
//! A component is built once against the `duckdb:extension` WIT world and runs
//! identically here and under the standalone `ducklink` host (Direction 1).
//! Both directions share [`ducklink_runtime`]:
//!   - `ducklink_runtime::duckdb_extension_bindings` — the wasmtime `bindgen!`
//!     for the component ABI (instantiate, run `load()`, dispatch callbacks).
//!   - `ducklink_runtime::reg` — the neutral capture model: what a component's
//!     `load()` registered (scalars, tables, aggregates, …), free of any
//!     direction-specific loader types.
//!   - `ducklink_runtime::CallbackRegistry` — maps a DuckDB-side invocation
//!     handle back to the owning component + its guest dispatcher.
//!
//! The flow for `CALL ducklink_load('x.wasm')`:
//!   1. Instantiate the component and run `load()` against a host store that
//!      implements the generated `Host*` capability traits, capturing each
//!      registration as a `reg::*` record (the **shared capture** path — see the
//!      Status note below).
//!   2. For each captured `reg::ScalarReg`, register a DuckDB scalar function
//!      whose per-row callback re-enters wasmtime via the `CallbackRegistry`,
//!      marshalling `reg::DuckValue` <-> DuckDB vectors.
//!
//! # Status
//!
//! This is the submission scaffold. The shared capture store-state currently
//! lives in `ducklink-host` (`ExtensionStoreState` + its `Host*` impls), bound
//! to the Direction-1 `CoreExecution`. Lifting it into `ducklink-runtime` behind
//! a sink trait (so this crate supplies a C-API sink) is the remaining engine
//! extraction; until then the entrypoint below registers the control surface
//! and the dispatch design is wired but the dynamic per-row bridge is a TODO.

use std::error::Error;

use duckdb::Connection;
use duckdb_loadable_macros::duckdb_entrypoint_c_api;
use libduckdb_sys as ffi;

// Re-exported so the rest of the crate (and future modules) build the bridge
// against exactly the shared types the host uses.
use ducklink_runtime::reg;
use ducklink_runtime::CallbackRegistry;

mod component;

/// Loadable-extension entry point. DuckDB calls this when `LOAD ducklink` runs.
///
/// Registers the `ducklink_load` control function. Calling it loads a component
/// and registers the functions it declares into the active connection.
#[duckdb_entrypoint_c_api]
pub unsafe fn ducklink_init(con: Connection) -> Result<(), Box<dyn Error>> {
    component::register_loader(&con)?;
    Ok(())
}

/// Marshals a DuckDB scalar argument into the neutral value model the component
/// expects. (Vector-level marshalling lives in `component`; this is the scalar
/// unit the bridge is built from.)
#[allow(dead_code)]
fn duckdb_to_neutral(_v: &duckdb::types::Value) -> reg::DuckValue {
    // TODO: full type coverage once the C-API scalar bridge lands.
    reg::DuckValue::Null
}

/// A handle the loader keeps so a DuckDB invocation can be routed back to the
/// owning component instance through the shared registry.
#[allow(dead_code)]
struct LoaderState {
    callbacks: CallbackRegistry,
}
