//! Phase 2 (@5) integration test: `ATTACH '...' AS mydb (TYPE sqlitewasm)`
//! then `SELECT` roundtrips through the host's storage-dispatch shim.
//!
//! Phase 2d: the extension side of the rowid contract (ADR Amendment A5)
//! is now implemented (`extensions/sqlitewasm-component` advertises a
//! synthetic `rowid` column at index 0 in `storage_table_columns` and
//! emits SQLite's native ROWID in scans; see docs/at5-rowid-mechanism.md).
//! The test is no longer `#[ignore]`d.
//!
//! The test uses the SAME preflight-skip pattern as `cron_wasm_driver`:
//! if the required wasm artifacts have not been built (`make all` +
//! `make ext` + `make host`), the test SKIPS cleanly rather than failing.
//! That keeps `cargo test` on a fresh clone green while still exercising
//! the whole pipeline once artifacts are in place.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Repo root — one level up from the manifest dir of this crate.
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

/// Return None (test SKIPS) if the required build artifacts are absent;
/// return `Some((bin, sqlitewasm_ext))` otherwise.
fn preflight() -> Option<(PathBuf, PathBuf)> {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let sqlitewasm_ext = root.join("artifacts/extensions/sqlitewasm.wasm");
    let core_wasm = root.join("target/wasm32-wasip2/release/ducklink_core.wasm");
    let cli_wasm = root.join("target/wasm32-wasip2/release/ducklink_cli.wasm");
    for required in [&bin, &sqlitewasm_ext, &core_wasm, &cli_wasm] {
        if !required.exists() {
            eprintln!(
                "test_at5_read_sqlitewasm: skipping — missing {} \
                 (build with `make all && cargo component build \
                 -p sqlitewasm-component --target wasm32-wasip2 --release && \
                 mkdir -p artifacts/extensions && cp \
                 target/wasm32-wasip1/release/sqlitewasm.wasm artifacts/extensions/`)",
                required.display()
            );
            return None;
        }
    }
    Some((bin, sqlitewasm_ext))
}

/// Build a small SQLite file at `path` with schema `(id INTEGER, name TEXT)`
/// and three rows `(1,'a'), (42,'z'), (99,'x')`.
fn write_sample_sqlite_db(path: &Path) {
    use rusqlite::Connection;
    let conn = Connection::open(path).expect("create sqlite file");
    conn.execute_batch(
        "CREATE TABLE foo (id INTEGER, name TEXT); \
         INSERT INTO foo VALUES (1, 'a'), (42, 'z'), (99, 'x');",
    )
    .expect("populate foo");
}

/// Extract lines that look like CSV data rows (drop CLI banner/prompt noise).
fn count_data_rows(stdout: &[u8]) -> usize {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| {
            let t = l.trim();
            // A data row here looks like "1,a" / "42,z" / "99,x" — has a comma
            // and every non-punct char is either a digit, a comma, or an
            // ASCII letter.
            t.contains(',')
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b',' || b == b'.' || b == b'-')
        })
        .count()
}

#[test]
fn attach_sqlitewasm_and_select_roundtrip() {
    let Some((bin, _sqlitewasm_ext)) = preflight() else {
        return;
    };
    let root = repo_root();

    // Stage a SQLite file under target/ so wasi-fs's cwd preopen sees it.
    let target = root.join("target");
    std::fs::create_dir_all(&target).expect("create target dir");
    let workdir = tempfile::tempdir_in(&target).expect("tempdir under target");
    let db_path = workdir.path().join("read.db");
    write_sample_sqlite_db(&db_path);
    let db_rel = db_path.strip_prefix(&root).expect("tempdir under repo");
    let db_str = db_rel.to_str().expect("utf-8 tmp path");

    // ATTACH + SELECT * FROM mydb.foo — all-rows count.
    let all = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD sqlitewasm; \
                 ATTACH '{db_str}' AS mydb (TYPE sqlitewasm); \
                 SELECT id, name FROM mydb.foo;"
            ),
        ],
    );
    assert!(all.status.success(), "{}", dump("ATTACH + full SELECT failed", &all));
    let n_all = count_data_rows(&all.stdout);
    assert_eq!(
        n_all, 3,
        "{}",
        dump(
            &format!("expected 3 data rows from mydb.foo, got {n_all}"),
            &all
        )
    );

    // SELECT * FROM mydb.foo WHERE id = 42 — one-row filter.
    let filtered = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD sqlitewasm; \
                 ATTACH '{db_str}' AS mydb (TYPE sqlitewasm); \
                 SELECT id, name FROM mydb.foo WHERE id = 42;"
            ),
        ],
    );
    assert!(
        filtered.status.success(),
        "{}",
        dump("filtered SELECT failed", &filtered)
    );
    let stdout = String::from_utf8_lossy(&filtered.stdout);
    assert!(
        stdout.contains("42") && stdout.contains('z'),
        "{}",
        dump("filtered SELECT missing (42,z)", &filtered)
    );
}
