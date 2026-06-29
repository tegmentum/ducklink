//! `sqlitecompat` — SQLite built-in scalars DuckDB lacks, for DuckDB.
//!
//! THIN, GENERATED ducklink shim: a `wit_bindgen::generate!` block plus
//! one `datalink_extcore::duckdb_shim!` invocation. All logic + the
//! capability surface live ONCE in `sqlitecompat-core` (datalink). This
//! is the cross-compat (#153) DuckDB <- SQLite direction: the names are
//! SQLite builtins that DuckDB does not ship (`zeroblob`, `randomblob`,
//! `likely`, `unlikely`, `likelihood`), so loading this component gives a
//! DuckDB user the SQLite spellings + semantics. There is no sqlink
//! counterpart (SQLite already has these as builtins).

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::duckdb_shim! {
    core = sqlitecompat_core::Core;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
