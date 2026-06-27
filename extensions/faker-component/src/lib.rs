//! `faker` scalars for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `faker-core` (datalink); the
//! registration ABI, handle table, the six `call_*` arms, and the
//! `Duckvalue` marshalling are derived from the core's declaration.
//!
//! All generators are declared `nondeterministic` in the core, so the
//! generated registration omits `Funcflags::DETERMINISTIC` and DuckDB
//! treats them as volatile (the optimizer never folds them).

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = faker_core::Core;
    types = duckdb::extension::types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
