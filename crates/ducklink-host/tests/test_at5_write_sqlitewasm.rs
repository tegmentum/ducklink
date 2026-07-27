//! Phase 2 (@5) integration test: INSERT / UPDATE / DELETE against
//! `<alias>.<table>` when `<alias>` was ATTACHed with `(TYPE sqlitewasm)`.
//!
//! STATUS: `#[ignore]`d. The host-side write intercept is now COMPLETE
//! (see `HostState::intercept_write` + `ExtensionManager::dispatch_storage_
//! {insert,update,delete}_direct`). Phase 2c's Step 3 landed the Amendment
//! A5 rowid pre-scan: UPDATE/DELETE synthesize `SELECT * FROM
//! <alias>.main.<table> [WHERE <pred>]`, locate the extension-declared
//! `rowid` column, then dispatch `storage-write-dispatch.{update,delete}`
//! with the collected rowids (and merged full-row payload for UPDATE).
//!
//! The remaining blocker is identical to `test_at5_read_sqlitewasm`: the
//! shipped `extensions/sqlitewasm-component` still targets
//! `duckdb:extension@4.0.0` and depends on the out-of-workspace
//! `datalink-extcore` crate. Porting sqlitewasm to @5 (or building a mock
//! storage-write-dispatch component) is a Phase 2d task.
//!
//! Run with `cargo test -p ducklink-host -- --ignored` once sqlitewasm
//! ships at @5. Test bodies below describe the expected e2e flows the
//! landed host-side code already supports.

#[test]
#[ignore = "requires sqlitewasm-component rebuilt at duckdb:extension@5.0.0"]
fn insert_into_attached_dispatches_to_storage_write() {
    // Intended flow (implementation ready end-to-end on the host side):
    //   1. Create an empty SQLite file `test.db` with `CREATE TABLE foo
    //      (id INTEGER, name TEXT)`.
    //   2. `cli.execute("LOAD sqlitewasm; ATTACH 'test.db' AS mydb (TYPE sqlitewasm)")`.
    //   3. `cli.execute("INSERT INTO mydb.foo (id, name) VALUES (100, 'new')")`.
    //   4. `cli.execute("SELECT * FROM mydb.foo WHERE id = 100")` returns
    //      the new row.
    //   5. Reopen `test.db` with a native sqlite3 client; assert the row is
    //      visible (durability check).
    unreachable!("test body deferred to Phase 2d — see the module docs");
}

#[test]
#[ignore = "requires sqlitewasm-component rebuilt at duckdb:extension@5.0.0"]
fn update_attached_via_where_dispatches_after_rowid_prescan() {
    // Intended flow (Amendment A5 pre-scan is IMPLEMENTED; only extension is
    // still missing):
    //   1. Same setup as `insert_into_attached_dispatches_to_storage_write`.
    //   2. `cli.execute("UPDATE mydb.foo SET name = 'updated' WHERE id = 100")`
    //      triggers `SELECT * FROM mydb.main.foo WHERE id = 100` through the
    //      read path; the host extracts the rowid column, merges `name =
    //      'updated'` into the row payload, and dispatches
    //      `storage_write_update(catalog, table, rowids, rows)`.
    //   3. `cli.execute("SELECT * FROM mydb.foo WHERE id = 100")` confirms
    //      the new value.
    unreachable!("test body deferred to Phase 2d — see the module docs");
}

#[test]
#[ignore = "requires sqlitewasm-component rebuilt at duckdb:extension@5.0.0"]
fn delete_attached_via_where_dispatches_after_rowid_prescan() {
    // Intended flow (Amendment A5 pre-scan is IMPLEMENTED):
    //   1. Same setup.
    //   2. `cli.execute("DELETE FROM mydb.foo WHERE id = 100")` runs the
    //      same pre-scan as UPDATE, then dispatches
    //      `storage_write_delete(catalog, table, rowids)`.
    //   3. `cli.execute("SELECT * FROM mydb.foo WHERE id = 100")` returns 0
    //      rows.
    unreachable!("test body deferred to Phase 2d — see the module docs");
}
