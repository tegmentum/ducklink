//! `stdsql` cross-dialect standard-SQL scalars for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `stdsql-core` (datalink). Only the
//! spellings DuckDB does NOT ship as builtins are declared there (space,
//! initcap, the ClickHouse camelCase family, and the PostgreSQL to_*/
//! quote_*/byte accessors); DuckDB's own greatest/least/left/right/lpad/
//! rpad/repeat/translate/to_hex/bit_length/chr/ascii/char_length/from_hex/
//! get_bit/set_bit stay the DB's own builtins, and `if` stays its reserved
//! CASE syntax.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = stdsql_core::Core;
    types = duckdb::extension::types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
