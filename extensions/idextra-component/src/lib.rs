//! `idextra` scalars for DuckDB (ksuid / cuid2).
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus one
//! `datalink_extcore::duckdb_shim!` invocation. All logic + the capability
//! surface live ONCE in `idextra-core` (datalink); the registration ABI, handle
//! table, the dispatch arms (incl. the major-4 columnar hot path), and the
//! `Duckvalue` marshalling are derived from the core's declaration.
//!
//! ksuid / cuid2 are declared `nondeterministic` in the core, so the generated
//! registration omits `Funcflags::DETERMINISTIC` and DuckDB treats them as
//! volatile (the optimizer never folds them).

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = idextra_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
