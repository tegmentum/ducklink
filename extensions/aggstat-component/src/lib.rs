//! `aggstat` aggregates for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_agg_shim!` invocation. All logic + the
//! capability surface (the `harmonic_mean` aggregate's init/step/finalize
//! fold) live ONCE in `aggstat-core` (datalink); the registration ABI,
//! handle table, the six `call_*` arms, and the `Duckvalue` marshalling
//! are derived from the core's declaration.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_agg_shim! {
    core = aggstat_core::Core;
    types = duckdb::extension::types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
