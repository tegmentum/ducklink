//! Phase 2 (@5) integration test: `ATTACH '...' AS mydb (TYPE sqlitewasm)`
//! then `SELECT` roundtrips through the host's storage-dispatch shim.
//!
//! STATUS: `#[ignore]`d. The host-side ATTACH intercept + dispatch plumbing
//! is wired end-to-end in `ducklink-host` (see `HostState::intercept_attach`
//! + `ExtensionManager::dispatch_storage_*` + Phase 2c's per-table
//! `register-table-function` view materialization). The @5 read-path
//! machinery is ready.
//!
//! The remaining blocker is the extension side: the shipped
//! `extensions/sqlitewasm-component` still targets `duckdb:extension@4.0.0`
//! and depends on the out-of-workspace `datalink-extcore` crate, so it cannot
//! be loaded by the @5 host. Porting sqlitewasm to @5 is a Phase 2d task in
//! its own right (the write side flows through `storage-write-dispatch` and
//! must gain a synthetic `rowid` column per ADR Amendment A5).
//!
//! Run with `cargo test -p ducklink-host -- --ignored` once sqlitewasm ships
//! at @5 (or once a purpose-built mock storage-dispatch component lands in
//! `extensions/`).

#[test]
#[ignore = "requires sqlitewasm-component (or an equivalent mock) rebuilt at duckdb:extension@5.0.0"]
fn attach_sqlitewasm_and_select_roundtrip() {
    // Intended flow once the @5 sqlitewasm component (or a mock storage
    // extension) is available:
    //   1. Build a small SQLite file at `test.db` with `(id INTEGER, name TEXT)`
    //      populated with `(1,'a'), (42,'z'), (99,'x')`.
    //   2. Boot the ducklink CLI harness (see `cron_wasm_driver::run_ducklink`
    //      for the pattern), with cwd = repo root so
    //      `target/wasm32-wasip2/release/ducklink_core.wasm` resolves.
    //   3. `cli.execute("LOAD sqlitewasm;")`.
    //   4. `cli.execute("ATTACH 'test.db' AS mydb (TYPE sqlitewasm)")`.
    //   5. Assert `cli.execute("SELECT * FROM mydb.foo")` returns 3 rows in
    //      declared order.
    //   6. Assert `cli.execute("SELECT * FROM mydb.foo WHERE id = 42")`
    //      returns exactly the one matching row (correctness only -- ADR
    //      A2 Risk 2 documents the C-API-pushdown perf regression).
    unreachable!("test body deferred to Phase 2d — see the module docs");
}
