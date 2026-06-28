//! `talib` technical indicators for DuckDB, as WINDOW functions.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_agg_shim!` invocation. All logic + the
//! capability surface (the `sma` / `ema` / `rsi` aggregate folds over a
//! frame) live ONCE in `talib-core` (datalink); the registration ABI,
//! handle table, the `call_*` arms, and the `Duckvalue` marshalling are
//! derived from the core's declaration.
//!
//! These register as DuckDB aggregates, so DuckDB's window machinery
//! drives them over an `OVER (... ROWS BETWEEN ...)` frame (engine-
//! resolved frames): `sma(close) OVER (ORDER BY t ROWS BETWEEN 2
//! PRECEDING AND CURRENT ROW)` is a 3-period SMA. The SAME core also
//! drives the sqlink streaming window path (`sqlite_agg_shim!`).

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_agg_shim! {
    core = talib_core::Core;
    types = duckdb::extension::types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
