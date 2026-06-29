//! `chrono` cross-dialect datetime scalars for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `chrono-core` (datalink). Only the
//! dialect spellings DuckDB does NOT ship as builtins are declared there
//! (the MySQL/BigQuery/Snowflake date+timestamp aliases + tz-convert +
//! duration + business-day surface); year/month/day/date_part/date_trunc/
//! date_diff/epoch/make_*/last_day/age/now/to_timestamp/time_bucket stay
//! DuckDB's own builtins.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = chrono_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
