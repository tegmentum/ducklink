//! Bug 4 stress test: 10k INSERT statements against a sqlitewasm-backed
//! attach must NOT exhibit the quadratic full-DB serialize + full-file rewrite
//! behaviour that the pre-fix write-back path did after every mutation. The
//! post-fix path opts sqlitewasm in via `writes_persist_directly = true`,
//! opens a NATIVE file-backed rusqlite connection at ATTACH time, and replays
//! each write there directly -- turning N INSERTs from O(N * DB_size) into
//! O(N * row_size).
//!
//! Preflight-skip pattern (as in `test_at5_write_sqlitewasm`): if the required
//! wasm artifacts / binary are missing the test SKIPs so a fresh clone's
//! `cargo test` still passes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

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
        "{label}\n  status: {}\n  stdout:\n{}\n  stderr (tail):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        // The write-intercept prints a per-INSERT log line; at 10k rows the
        // stderr is huge and pointless on success. Tail 40 lines is enough
        // to see failure context.
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
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
                "test_at5_bug4_incremental_writeback: skipping — missing {}",
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

fn count_sqlite_rows(path: &Path) -> i64 {
    use rusqlite::Connection;
    let conn = Connection::open(path).expect("open sqlite");
    conn.query_row("SELECT COUNT(*) FROM foo", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Bug 4 regression: 10k INSERTs on the attached table finish in bounded
/// wall time, and every row lands in the underlying sqlite file. The wall-
/// time budget is deliberately loose (60s) so a slow / loaded CI machine
/// doesn't flake -- the point is that the OLD quadratic path would take
/// tens of minutes at 10k INSERTs (each write triggers a full serialize +
/// full-file rewrite of a database that keeps growing), so a sub-minute
/// bound is a strong smoke signal that the fast path is engaged.
#[test]
fn ten_thousand_inserts_are_not_quadratic() {
    let Some(bin) = preflight() else { return };
    let root = repo_root();

    let workdir = tempfile::tempdir_in(root.join("target")).expect("tempdir");
    let db_path = workdir.path().join("stress10k.db");
    write_empty_sqlite_db(&db_path);
    let db_rel = db_path.strip_prefix(&root).expect("tempdir under repo");
    let db_str = db_rel.to_str().unwrap();

    // Build a single SQL script with 10k INSERT statements to avoid the
    // per-invocation startup cost. Each INSERT is its own statement (the
    // AT5 write intercept processes them one at a time), which is exactly
    // the shape that would have been quadratic under the old serialize +
    // rewrite path.
    const N: usize = 10_000;
    let mut sql = String::with_capacity(64 * (N + 4));
    sql.push_str("LOAD sqlitewasm; ");
    sql.push_str(&format!(
        "ATTACH '{db_str}' AS mydb (TYPE sqlitewasm); "
    ));
    for i in 0..N {
        sql.push_str(&format!(
            "INSERT INTO mydb.foo (id, name) VALUES ({i}, 'row-{i}'); "
        ));
    }

    let start = Instant::now();
    let out = run_ducklink(&bin, &root, &["sql", &sql]);
    let elapsed = start.elapsed();
    assert!(out.status.success(), "{}", dump("10k INSERT failed", &out));

    let count = count_sqlite_rows(&db_path);
    assert_eq!(
        count, N as i64,
        "expected {N} rows in the underlying sqlite file, got {count}"
    );

    // Loose upper bound: the pre-fix path would rewrite a growing DB file
    // N times (O(N^2) fs bytes written); the fast path is O(N) native
    // sqlite INSERTs on a persistent file connection. Sub-minute here is
    // a robust "the fast path is engaged" signal that stays green on the
    // slowest CI runners.
    assert!(
        elapsed.as_secs() < 60,
        "10k INSERTs took {}s -- expected < 60s. Fast path likely NOT \
         engaged (regression). {}",
        elapsed.as_secs(),
        dump("timing", &out)
    );

    eprintln!(
        "[bug4-stress] 10k INSERTs against sqlitewasm attach: {}ms \
         ({} rows/s), {} bytes written to {}",
        elapsed.as_millis(),
        (N as f64 / elapsed.as_secs_f64()).round() as u64,
        std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0),
        db_path.display()
    );
}
