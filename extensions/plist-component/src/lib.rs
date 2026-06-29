//! Apple property-list (plist) parsing for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus one
//! `datalink_extcore::duckdb_shim!` invocation. The parse + JSON-render logic +
//! the capability surface (plist_to_json / plist_get) live ONCE in `plist-core`
//! (datalink); the registration ABI, handle table, the dispatch arms (incl. the
//! major-4 columnar hot path), and the `Duckvalue` marshalling are derived from
//! the core's declaration.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = plist_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
