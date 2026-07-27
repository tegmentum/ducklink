//! Phase 2 (@5) integration test: INSERT / UPDATE / DELETE against
//! `<alias>.<table>` when `<alias>` was ATTACHed with `(TYPE sqlitewasm)`.
//!
//! Phase 2d: the extension side of the storage-write-dispatch contract
//! is now implemented (`extensions/sqlitewasm-component` exports
//! `duckdb-extension-storage-write` and translates
//! insert/update/delete/create-table into SQLite DML/DDL; see
//! docs/at5-rowid-mechanism.md). The tests are no longer `#[ignore]`d.
//!
//! Preflight-skip pattern (as in `cron_wasm_driver` / test_at5_read):
//! if the required wasm artifacts are missing the tests SKIP so a
//! fresh clone's `cargo test` still passes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

fn run_ducklink(bin: &Path, root: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("spawn ducklink {args:?}: {e}"))
}

fn dump(label: &str, out: &Output) -> String {
    format!(
        "{label}\n  status: {}\n  stdout:\n{}\n  stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

fn preflight() -> Option<PathBuf> {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let sqlitewasm_ext = root.join("artifacts/extensions/sqlitewasm.wasm");
    let core_wasm = root.join("target/wasm32-wasip2/release/ducklink_core.wasm");
    let cli_wasm = root.join("target/wasm32-wasip2/release/ducklink_cli.wasm");
    for required in [&bin, &sqlitewasm_ext, &core_wasm, &cli_wasm] {
        if !required.exists() {
            eprintln!(
                "test_at5_write_sqlitewasm: skipping — missing {} \
                 (build with `make all && cargo component build \
                 -p sqlitewasm-component --target wasm32-wasip2 --release && \
                 mkdir -p artifacts/extensions && cp \
                 target/wasm32-wasip1/release/sqlitewasm.wasm artifacts/extensions/`)",
                required.display()
            );
            return None;
        }
    }
    Some(bin)
}

fn write_empty_sqlite_db(path: &Path) {
    use rusqlite::Connection;
    let conn = Connection::open(path).expect("create sqlite file");
    conn.execute_batch("CREATE TABLE foo (id INTEGER, name TEXT);")
        .expect("create foo");
}

fn read_sqlite_row(path: &Path, id: i64) -> Option<(i64, String)> {
    use rusqlite::Connection;
    let conn = Connection::open(path).ok()?;
    conn.query_row("SELECT id, name FROM foo WHERE id = ?", [id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })
    .ok()
}

fn count_sqlite_rows(path: &Path, id: i64) -> i64 {
    use rusqlite::Connection;
    let conn = Connection::open(path).expect("open sqlite");
    conn.query_row("SELECT COUNT(*) FROM foo WHERE id = ?", [id], |r| r.get(0))
        .unwrap_or(0)
}

#[test]
fn insert_into_attached_dispatches_to_storage_write() {
    let Some(bin) = preflight() else { return };
    let root = repo_root();

    let workdir = tempfile::tempdir_in(root.join("target")).expect("tempdir");
    let db_path = workdir.path().join("insert.db");
    write_empty_sqlite_db(&db_path);
    let db_rel = db_path.strip_prefix(&root).expect("tempdir under repo");
    let db_str = db_rel.to_str().unwrap();

    let ins = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD sqlitewasm; \
                 ATTACH '{db_str}' AS mydb (TYPE sqlitewasm); \
                 INSERT INTO mydb.foo (id, name) VALUES (100, 'new');"
            ),
        ],
    );
    assert!(ins.status.success(), "{}", dump("INSERT failed", &ins));

    // Re-open with a native sqlite3 client — the write must be durable in the
    // underlying file, not just visible through the extension's cache.
    let row = read_sqlite_row(&db_path, 100);
    assert_eq!(
        row,
        Some((100, "new".to_string())),
        "expected (100,'new') to be present in the underlying SQLite file"
    );
}

#[test]
fn update_attached_via_where_dispatches_after_rowid_prescan() {
    let Some(bin) = preflight() else { return };
    let root = repo_root();

    let workdir = tempfile::tempdir_in(root.join("target")).expect("tempdir");
    let db_path = workdir.path().join("update.db");
    write_empty_sqlite_db(&db_path);
    let db_rel = db_path.strip_prefix(&root).expect("tempdir under repo");
    let db_str = db_rel.to_str().unwrap();

    let upd = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD sqlitewasm; \
                 ATTACH '{db_str}' AS mydb (TYPE sqlitewasm); \
                 INSERT INTO mydb.foo (id, name) VALUES (100, 'new'); \
                 UPDATE mydb.foo SET name = 'updated' WHERE id = 100;"
            ),
        ],
    );
    assert!(upd.status.success(), "{}", dump("UPDATE failed", &upd));

    let row = read_sqlite_row(&db_path, 100);
    assert_eq!(
        row,
        Some((100, "updated".to_string())),
        "expected UPDATE to change name to 'updated'"
    );
}

#[test]
fn delete_attached_via_where_dispatches_after_rowid_prescan() {
    let Some(bin) = preflight() else { return };
    let root = repo_root();

    let workdir = tempfile::tempdir_in(root.join("target")).expect("tempdir");
    let db_path = workdir.path().join("delete.db");
    write_empty_sqlite_db(&db_path);
    let db_rel = db_path.strip_prefix(&root).expect("tempdir under repo");
    let db_str = db_rel.to_str().unwrap();

    let del = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD sqlitewasm; \
                 ATTACH '{db_str}' AS mydb (TYPE sqlitewasm); \
                 INSERT INTO mydb.foo (id, name) VALUES (100, 'new'); \
                 DELETE FROM mydb.foo WHERE id = 100;"
            ),
        ],
    );
    assert!(del.status.success(), "{}", dump("DELETE failed", &del));

    assert_eq!(
        count_sqlite_rows(&db_path, 100),
        0,
        "expected DELETE to remove row with id=100"
    );
}
