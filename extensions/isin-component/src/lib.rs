//! ISIN (ISO 6166) securities-identifier scalars for DuckDB.
//!
//! This file is the THIN, GENERATED ducklink shim for the `isin`
//! extension: a `wit_bindgen::generate!` block plus one
//! `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `isin-core` (datalink); the
//! registration ABI, the `u32` handle table, the six `call_*` arms, and
//! the `Duckvalue` marshalling are derived from the core's declaration.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = isin_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
