//! `sys_compat` cross-DB system / identity scalars for DuckDB.
//!
//! BI tools and ORMs probe with `SELECT version()` / `SELECT current_user`
//! to identify the engine and stop asking once they see a non-NULL answer.
//! DuckDB already provides `version()`, `current_user`, `session_user`,
//! `user`, `current_role`, `current_database()`, `current_schema()`,
//! `current_schemas()` and `format_bytes()` as builtins, so those are NOT
//! re-registered (re-registering a same-signature builtin is LOAD-FATAL).
//!
//! # Why the `declare!` table lives HERE, not in a datalink core
//!
//! Unlike `stdsql`/`stats`, these functions carry NO portable shared
//! logic — each returns the *engine's own identity*, which differs per DB
//! (sqlink answers `'sqlink'`/`'main'`; DuckDB answers `'duckdb'`/
//! `'memory'`/`'main'`). A shared neutral core would have to bake one DB's
//! constants, defeating the point. So this component is DB-PRIVATE: the
//! `declare!` table is defined inline with DuckDB-appropriate constants,
//! and it still rides the `duckdb_shim!` codegen for the registration ABI
//! and value marshalling. sqlink keeps its own hand-written `sys-compat`.
//!
//! # The genuine gaps (DuckDB-appropriate constants)
//!
//!   * `system_user()`   -> `'duckdb'`  (matches DuckDB's `current_user`)
//!   * `database()`      -> `'memory'`  (matches `current_database()` for
//!                                       an in-memory DB; the default DB)
//!   * `schema()`        -> `'main'`    (matches `current_schema()`)
//!   * `collation(text)` -> `'BINARY'`  (DuckDB compares VARCHAR byte-wise
//!                                       by default = a binary collation)

extern crate alloc;

use datalink_extcore::NeutralValue;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

datalink_extcore::declare! {
    core = SysCompatCore;
    extension = "sys_compat";
    version = env!("CARGO_PKG_VERSION");

    // The OS/engine user. DuckDB's current_user is 'duckdb'; match it.
    scalar system_user() -> text [propagate, deterministic] = |_a| {
        Ok(NeutralValue::Text("duckdb".into()))
    };
    // Bare database()/schema() aliases (DuckDB only ships the current_* forms).
    scalar database() -> text [propagate, deterministic] = |_a| {
        Ok(NeutralValue::Text("memory".into()))
    };
    scalar schema() -> text [propagate, deterministic] = |_a| {
        Ok(NeutralValue::Text("main".into()))
    };
    // collation(expr) -> the effective collation name. DuckDB's default is
    // byte-wise comparison; report it as BINARY (a constant, like sqlink).
    scalar collation(text) -> text [propagate, deterministic] = |_a| {
        Ok(NeutralValue::Text("BINARY".into()))
    };
}

datalink_extcore::duckdb_shim! {
    core = SysCompatCore;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}
