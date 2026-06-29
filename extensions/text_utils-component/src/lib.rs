//! `text-utils` cross-dialect string scalars for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `text-utils-core` (datalink). Only the
//! functions DuckDB does NOT already provide as builtins are declared
//! there (`sql_normalize`, `insert`, `locate(2)`, `locate(3)`);
//! position/split_part/lcase/ucase/split/string_split/str_split/reverse
//! are DuckDB builtins, and the `prefixes` table function stays
//! DB-private.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = text_utils_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
