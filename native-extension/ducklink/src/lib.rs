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

/// The Direction-2 DuckDB sink (registration + dispatch). Present whenever the
/// duckdb crate is available (the `loadable` and `bundled` features both enable
/// it); the `bundled` end-to-end test lives in this module.
#[cfg(feature = "duckdb-api")]
pub mod reg_duckdb;

#[cfg(feature = "loadable")]
mod loadable {
    use std::error::Error;
    use std::sync::{Arc, Mutex};

    use duckdb::Connection;
    use duckdb_loadable_macros::duckdb_entrypoint_c_api;

    use crate::engine::Engine2;
    use crate::reg_duckdb::{component_specs_from_env, register_components};

    /// Loadable-extension entry point. DuckDB calls this `ducklink_init_c_api`
    /// when `LOAD ducklink` runs.
    ///
    /// Loads every component named in the `DUCKLINK_COMPONENTS` environment
    /// variable (a `:`-separated list of `name=path` or `path`) and registers
    /// their scalar functions into the catalog, so they are usable from SQL:
    ///
    /// ```sh
    /// DUCKLINK_COMPONENTS=sample=/path/sample_extension.wasm \
    ///   duckdb -unsigned -c "LOAD 'ducklink.duckdb_extension'; SELECT sample_plus_one(41);"
    /// ```
    ///
    /// The shared `Engine2` is kept alive by the `Arc` cloned into each
    /// registered function's state.
    #[duckdb_entrypoint_c_api(ext_name = "ducklink", min_duckdb_version = "v1.0.0")]
    pub fn ducklink_init(con: Connection) -> Result<(), Box<dyn Error>> {
        let engine = Arc::new(Mutex::new(Engine2::new().map_err(stringify)?));
        let specs = component_specs_from_env();
        let registered =
            // No raw connection here (the entry point only receives a duckdb-rs
            // Connection), so aggregate functions are skipped with a note; scalar
            // and table functions register fine.
            register_components(&con, None, engine, &specs).map_err(stringify)?;
        eprintln!(
            "[ducklink] loaded {} component(s); registered {registered} scalar function(s)",
            specs.len()
        );
        Ok(())
    }

    fn stringify(err: impl std::fmt::Display) -> Box<dyn Error> {
        err.to_string().into()
    }
}
