//! Phase 2 (@5) integration test: INSERT / UPDATE / DELETE against
//! `<alias>.<table>` when `<alias>` was ATTACHed with `(TYPE sqlitewasm)`.
//!
//! STATUS: `#[ignore]`d in this build for the same reason as
//! `test_at5_read_sqlitewasm.rs`: the ATTACH intercept + INSERT dispatch
//! path are wired end-to-end in `HostState::intercept_write` +
//! `ExtensionManager::dispatch_storage_insert_direct`, but exercising them
//! from a real CLI harness requires a sqlitewasm-component built against
//! the @5 WIT surface (the artifact on main is @4 and rejects the @5 host).
//!
//! UPDATE/DELETE additionally require the ADR Amendment A5 rowid pre-scan,
//! which in turn requires the Phase-2b materialized read path. Both are
//! deferred to Phase 2b -- v0's `intercept_write` returns a clear
//! `Unsupported(reason)` for UPDATE/DELETE against attached tables so no
//! caller silently gets wrong behavior.
//!
//! Run with `cargo test -p ducklink-host -- --ignored` once Phase 2b lands.

#[test]
#[ignore = "requires sqlitewasm-component rebuilt at @5"]
fn insert_into_attached_dispatches_to_storage_write() {
    // Intended flow once sqlitewasm is @5:
    //   1. Create an empty SQLite file `test.db` with `CREATE TABLE foo
    //      (id INTEGER, name TEXT)`.
    //   2. `cli.execute("LOAD sqlitewasm; ATTACH 'test.db' AS mydb (TYPE sqlitewasm)")`.
    //   3. `cli.execute("INSERT INTO mydb.foo (id, name) VALUES (1, 'x')")`.
    //   4. Reopen `test.db` with a native sqlite3 client; assert the row is
    //      visible.
    unreachable!("test body deferred to Phase 2b — see the module docs");
}

#[test]
#[ignore = "requires ADR Amendment A5 rowid pre-scan (Phase 2b)"]
fn update_attached_via_where_dispatches_after_rowid_prescan() {
    // Intended flow: `cli.execute("UPDATE mydb.foo SET name = 'y' WHERE id = 1")`
    // triggers a `SELECT rowid FROM mydb.foo WHERE id = 1` through the
    // materialized read path, collects the rowid(s), then dispatches
    // `storage_write_update(handle, catalog, table, rowids, new_values)`.
    unreachable!("test body deferred to Phase 2b — see the module docs");
}

#[test]
#[ignore = "requires ADR Amendment A5 rowid pre-scan (Phase 2b)"]
fn delete_attached_via_where_dispatches_after_rowid_prescan() {
    // `cli.execute("DELETE FROM mydb.foo WHERE id = 1")` triggers the same
    // pre-scan + rowid-list dispatch as UPDATE.
    unreachable!("test body deferred to Phase 2b — see the module docs");
}
