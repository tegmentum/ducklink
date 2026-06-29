//! Number formatting (thousands-grouping + SI prefixes) for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus one
//! `datalink_extcore::duckdb_shim!` invocation. All logic + the capability
//! surface live ONCE in `numfmt-core` (datalink); the registration ABI, handle
//! table, the dispatch arms (incl. the major-4 columnar hot path), and the
//! `Duckvalue` marshalling are derived from the core's declaration.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = numfmt_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
