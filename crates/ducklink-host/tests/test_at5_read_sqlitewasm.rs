//! Phase 2 (@5) integration test: `ATTACH '...' AS mydb (TYPE sqlitewasm)`
//! then `SELECT` roundtrips through the host's storage-dispatch shim.
//!
//! STATUS: `#[ignore]`d in this build. The ATTACH intercept + dispatch
//! plumbing is wired end-to-end in `ducklink-host` (see
//! `HostState::intercept_attach` + `ExtensionManager::dispatch_storage_*`),
//! but making SELECT roundtrip requires:
//!   1. A `sqlitewasm-component` .wasm built against the @5 WIT surface
//!      (the artifact under `artifacts/extensions/sqlitewasm.wasm` in the
//!      main checkout is @4 and cannot be loaded by the @5 host).
//!   2. The read-path MATERIALIZATION (ADR Decision 3 second half): populate
//!      an in-memory attached DB on the core from `storage_scan_open/next/
//!      close` output so `SELECT * FROM mydb.foo` resolves against real
//!      tables rather than requiring a `duckdb_create_table_function`
//!      registration path routed through the host as a synthetic extension.
//!      Deferred to a Phase-2b follow-up per the coordinator's guidance
//!      (see this session's report).
//!
//! Run with `cargo test -p ducklink-host -- --ignored` once (1) and (2) land.

#[test]
#[ignore = "requires sqlitewasm-component rebuilt at @5 and Phase-2b materialized-scan read path"]
fn attach_sqlitewasm_and_select_roundtrip() {
    // Intentional no-op body. When Phase 2b lands, replace with the
    // e2e flow described in ADR Decision 3 + Amendment A1:
    //   1. Build a small SQLite file at `test.db` with `(id INTEGER, name TEXT)`
    //      populated with a handful of rows.
    //   2. `let cli = CliHarness::new(&[...], &[...])?;`
    //   3. `cli.execute("LOAD sqlitewasm;")` -- requires sqlitewasm.wasm.
    //   4. `cli.execute("ATTACH 'test.db' AS mydb (TYPE sqlitewasm)")`.
    //   5. Assert `cli.execute("SELECT * FROM mydb.foo")` returns the rows.
    //   6. Assert `cli.execute("SELECT * FROM mydb.foo WHERE id = 42")`
    //      returns the filtered row (correctness only -- the C-API pushdown
    //      gap in ADR A2 Risk 2 means every row still crosses the boundary).
    unreachable!("test body deferred to Phase 2b — see the module docs");
}
