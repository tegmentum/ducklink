//! Stop-word detection + removal as DuckDB scalars (is_stopword /
//! remove_stopwords).
//!
//! THIN, GENERATED ducklink shim: `wit_bindgen::generate!` + one
//! `datalink_extcore::duckdb_shim!`. All logic + the capability surface live
//! ONCE in `stopwords-core` (datalink).

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = stopwords_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
