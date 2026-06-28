//! `math` cross-dialect scalars for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `math-core` (datalink). Only the
//! functions DuckDB does NOT already provide as builtins are declared
//! there (`exp2`, `e`, `rand`, `div`, `truncate(x)`, `truncate(x, n)`);
//! the trig/log/rounding family is DuckDB's own `core_functions`.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = math_core::Core;
    types = duckdb::extension::types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
