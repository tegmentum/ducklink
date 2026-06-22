//! ducklink — a native-DuckDB loadable extension that embeds wasmtime to run
//! `duckdb:extension` WebAssembly components (Direction 2).
//!
//! A component is built once against the `duckdb:extension` WIT world and runs
//! identically here and under the standalone `ducklink` host (Direction 1).
//! Both directions share [`ducklink_runtime`] — the bindgen, the neutral
//! `reg::*` capture model, the callback registry, and `load_component`.
//!
//! - [`engine`] is the direction-agnostic glue (load a component, capture its
//!   functions, dispatch invocations back into it). It depends only on
//!   `ducklink-runtime` + wasmtime, so it builds without the DuckDB toolchain.
//! - The `loadable` module (behind the `loadable` feature) is the DuckDB C-API
//!   binding: the extension entry point + the per-function registration that
//!   maps an [`engine::ScalarFunc`] onto a DuckDB scalar function whose callback
//!   re-enters [`engine::Engine2::dispatch_scalar`].

pub mod engine;

#[cfg(feature = "loadable")]
mod loadable {
    use std::error::Error;

    use duckdb::Connection;
    use duckdb_loadable_macros::duckdb_entrypoint_c_api;

    /// Loadable-extension entry point. DuckDB calls this when `LOAD ducklink`
    /// runs. Registers the `ducklink_load` control function.
    #[duckdb_entrypoint_c_api]
    pub unsafe fn ducklink_init(con: Connection) -> Result<(), Box<dyn Error>> {
        // TODO (native build): register a `ducklink_load(path VARCHAR)` function.
        // On call it uses a process-wide `engine::Engine2` to load the component
        // and, for each returned `engine::ScalarFunc`, registers a DuckDB scalar
        // function via the C API (`duckdb_create_scalar_function` +
        // `set_extra_info` carrying the `callback_handle`); that callback unpacks
        // the input vector to `reg::DuckValue`s, calls `Engine2::dispatch_scalar`,
        // and writes the result back into the output vector.
        let _ = con;
        Ok(())
    }
}
