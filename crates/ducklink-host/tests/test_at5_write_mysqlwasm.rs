//! Phase 2 (@5) integration test for mysqlwasm INSERT / UPDATE / DELETE
//! through the storage-dispatch + storage-write-dispatch AT5 write path
//! (Amendment A5). Mirrors `test_at5_write_sqlitewasm.rs`; where the
//! sqlitewasm version stages an on-disk SQLite file, this one stages a live
//! MySQL / MariaDB table addressed by the `MYSQL_TEST_URL` env var.
//!
//! Preflight-skip pattern (as in `test_at5_write_sqlitewasm.rs`,
//! `cron_wasm_driver`, `test_at5_read_sqlitewasm.rs`): if the required wasm
//! artifacts are missing OR `MYSQL_TEST_URL` is unset, every test SKIPS
//! cleanly so a fresh clone's `cargo test` stays green. To enable locally:
//!
//!     export MYSQL_TEST_URL=mysql://root:root@127.0.0.1:3306/ducktest
//!     export DUCKLINK_NETWORK_GRANT=mysqlwasm
//!     make all
//!     cargo component build -p mysqlwasm-component \
//!         --target wasm32-wasip2 --release
//!     mkdir -p artifacts/extensions
//!     cp target/wasm32-wasip1/release/mysqlwasm.wasm artifacts/extensions/
//!     cargo test -p ducklink-host --test test_at5_write_mysqlwasm -- --nocapture
//!
//! Each test provisions its own uniquely-named table under the target
//! database, exercises one write shape (INSERT / UPDATE / DELETE), and
//! DROPs the table on the way out (best-effort; a hard failure still
//! reports the assertion error, not the cleanup error).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

fn run_ducklink(bin: &Path, root: &Path, args: &[&str], grant: &str) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(root)
        // The MySQL wire client uses wasi:sockets; the host gates network
        // access behind DUCKLINK_NETWORK_GRANT. Set to the extension's name so
        // the grant matches.
        .env("DUCKLINK_NETWORK_GRANT", grant)
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

/// Return `Some((ducklink_bin, mysql_url))` if we can run; `None` if we must
/// skip. Skipping is silent by design -- CI without a live MySQL / without
/// the wasm artifacts stays green.
fn preflight() -> Option<(PathBuf, String)> {
    let url = match std::env::var("MYSQL_TEST_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!(
                "test_at5_write_mysqlwasm: skipping -- MYSQL_TEST_URL not set \
                 (set to e.g. mysql://root:root@127.0.0.1:3306/ducktest and \
                 export DUCKLINK_NETWORK_GRANT=mysqlwasm to enable)"
            );
            return None;
        }
    };
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let mysqlwasm_ext = root.join("artifacts/extensions/mysqlwasm.wasm");
    let core_wasm = root.join("target/wasm32-wasip2/release/ducklink_core.wasm");
    let cli_wasm = root.join("target/wasm32-wasip2/release/ducklink_cli.wasm");
    for required in [&bin, &mysqlwasm_ext, &core_wasm, &cli_wasm] {
        if !required.exists() {
            eprintln!(
                "test_at5_write_mysqlwasm: skipping -- missing {} \
                 (build with `make all && cargo component build \
                 -p mysqlwasm-component --target wasm32-wasip2 --release && \
                 mkdir -p artifacts/extensions && cp \
                 target/wasm32-wasip1/release/mysqlwasm.wasm \
                 artifacts/extensions/`)",
                required.display()
            );
            return None;
        }
    }
    Some((bin, url))
}

/// A unique table name for one test run: `test_at5_<what>_<pid>_<nanos>`.
/// The tests provision, use, and DROP this table on their own, so parallel
/// test runs against the same DB don't step on each other.
fn unique_table(what: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("test_at5_{what}_{pid}_{nanos}")
}

/// Drop the test table via `ducklink sql "ATTACH … LOAD … <DROP>"`. Best
/// effort -- failures print but do not fail the test (the assertion above
/// has already run).
fn drop_table(bin: &Path, root: &Path, url: &str, table: &str) {
    let out = run_ducklink(
        bin,
        root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 CALL mysqlwasm_raw('DROP TABLE IF EXISTS {table}');"
            ),
        ],
        "mysqlwasm",
    );
    if !out.status.success() {
        // The extension doesn't currently expose a raw-SQL escape hatch, so
        // this cleanup path may not run; leave a diagnostic breadcrumb.
        eprintln!(
            "test_at5_write_mysqlwasm: DROP TABLE {table} cleanup best-effort \
             (may leak; drop manually if a live-server run is repeated): \
             status={}",
            out.status
        );
    }
}

/// `SELECT id, name FROM <table> WHERE id = <id>` through ducklink itself.
/// Returns the first (id, name) tuple as text so the test can compare
/// against expected values without pulling in a native mysql client.
fn read_row_via_ducklink(
    bin: &Path,
    root: &Path,
    url: &str,
    table: &str,
    id: i64,
) -> String {
    let out = run_ducklink(
        bin,
        root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 SELECT id, name FROM mydb.{table} WHERE id = {id};"
            ),
        ],
        "mysqlwasm",
    );
    assert!(out.status.success(), "{}", dump("SELECT for verification", &out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `SELECT COUNT(*) FROM <table> WHERE id = <id>` via ducklink; returns the
/// stdout text unmodified (tests grep for '0' or '1').
fn count_rows_via_ducklink(
    bin: &Path,
    root: &Path,
    url: &str,
    table: &str,
    id: i64,
) -> String {
    let out = run_ducklink(
        bin,
        root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 SELECT COUNT(*) FROM mydb.{table} WHERE id = {id};"
            ),
        ],
        "mysqlwasm",
    );
    assert!(out.status.success(), "{}", dump("COUNT for verification", &out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn insert_into_attached_dispatches_to_storage_write() {
    let Some((bin, url)) = preflight() else { return };
    let root = repo_root();
    let table = unique_table("insert");

    // Provision the table via the extension's own CREATE TABLE path (dispatched
    // through storage-write-dispatch.create-table).
    let create = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 CREATE TABLE mydb.{table} (id INTEGER PRIMARY KEY, name TEXT);"
            ),
        ],
        "mysqlwasm",
    );
    assert!(create.status.success(), "{}", dump("CREATE TABLE failed", &create));

    let ins = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 INSERT INTO mydb.{table} (id, name) VALUES (100, 'new');"
            ),
        ],
        "mysqlwasm",
    );
    assert!(ins.status.success(), "{}", dump("INSERT failed", &ins));

    let stdout = read_row_via_ducklink(&bin, &root, &url, &table, 100);
    assert!(
        stdout.contains("100") && stdout.contains("new"),
        "expected (100,'new') in ducklink SELECT output; got:\n{stdout}"
    );

    drop_table(&bin, &root, &url, &table);
}

#[test]
fn update_attached_via_where_dispatches_after_rowid_prescan() {
    let Some((bin, url)) = preflight() else { return };
    let root = repo_root();
    let table = unique_table("update");

    let create = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 CREATE TABLE mydb.{table} (id INTEGER PRIMARY KEY, name TEXT);"
            ),
        ],
        "mysqlwasm",
    );
    assert!(create.status.success(), "{}", dump("CREATE TABLE failed", &create));

    let upd = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 INSERT INTO mydb.{table} (id, name) VALUES (100, 'new'); \
                 UPDATE mydb.{table} SET name = 'updated' WHERE id = 100;"
            ),
        ],
        "mysqlwasm",
    );
    assert!(upd.status.success(), "{}", dump("UPDATE failed", &upd));

    let stdout = read_row_via_ducklink(&bin, &root, &url, &table, 100);
    assert!(
        stdout.contains("100") && stdout.contains("updated"),
        "expected (100,'updated') after UPDATE; got:\n{stdout}"
    );

    drop_table(&bin, &root, &url, &table);
}

#[test]
fn delete_attached_via_where_dispatches_after_rowid_prescan() {
    let Some((bin, url)) = preflight() else { return };
    let root = repo_root();
    let table = unique_table("delete");

    let create = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 CREATE TABLE mydb.{table} (id INTEGER PRIMARY KEY, name TEXT);"
            ),
        ],
        "mysqlwasm",
    );
    assert!(create.status.success(), "{}", dump("CREATE TABLE failed", &create));

    let del = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD mysqlwasm; ATTACH '{url}' AS mydb (TYPE mysqlwasm); \
                 INSERT INTO mydb.{table} (id, name) VALUES (100, 'new'); \
                 DELETE FROM mydb.{table} WHERE id = 100;"
            ),
        ],
        "mysqlwasm",
    );
    assert!(del.status.success(), "{}", dump("DELETE failed", &del));

    let stdout = count_rows_via_ducklink(&bin, &root, &url, &table, 100);
    // COUNT should render as 0 somewhere in the output.
    assert!(
        stdout.contains('0'),
        "expected COUNT(*) = 0 after DELETE; got:\n{stdout}"
    );

    drop_table(&bin, &root, &url, &table);
}
