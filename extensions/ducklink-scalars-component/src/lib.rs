//! Ducklink control-plane scalars component: `ducklink_version()` and
//! `ducklink_help(name)`.
//!
//! Thin shim. All logic and the capability declaration live in
//! `ducklink-scalars-core` (datalink); the `duckdb_shim!` macro derives
//! the registration ABI, handle table, dispatch arms, and Duckvalue
//! marshalling from the core's `declare!`.
//!
//! Committed as part of the shared surface in
//! `ducklink-extension/STABILITY.md § 1.1`. Autoloaded by ducklink-host
//! (see `DUCKLINK_AUTOLOAD` default).

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = ducklink_scalars_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
