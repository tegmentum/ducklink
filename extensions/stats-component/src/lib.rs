//! `stats` percentile aggregates for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_agg_shim!` invocation. All logic + the
//! capability surface (the percentile/percentile_cont/percentile_disc
//! aggregates' init/step/finalize folds) live ONCE in `stats-core`
//! (datalink); the registration ABI, handle table, the `call_aggregate`
//! arm, and the `Duckvalue` marshalling are derived from the declaration.
//! DuckDB's own stddev/variance/median/mode/corr/covar/regr_*/skewness/
//! kurtosis/bit_*/any_value/array_agg/string_agg aggregates stay builtins.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_agg_shim! {
    core = stats_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
