//! `list` JSON-array-backed scalars for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `list-core` (datalink). Only the
//! PostgreSQL-flavour names DuckDB does NOT ship as builtins are declared
//! there (array_remove, list_length, the array_* reductions, array_dims/
//! lower/upper/ndims, array_positions, array_replace, arrays_overlap); the
//! rich native-LIST list_*/array_* family stays DuckDB's own builtins.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = list_core::Core;
    types = duckdb::extension::types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
