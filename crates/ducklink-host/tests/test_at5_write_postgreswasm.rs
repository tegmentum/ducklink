//! Phase 2 (@5) integration test: INSERT / UPDATE / DELETE against
//! `<alias>.<table>` when `<alias>` was ATTACHed with `(TYPE postgreswasm)`.
//!
//! The extension side (`extensions/postgreswasm-component`) exports
//! `duckdb-extension-storage-write` and translates INSERT to
//! `INSERT ... VALUES (...)`, UPDATE/DELETE to
//! `... WHERE ctid = '(block,offset)'::tid` after the host's rowid
//! pre-scan resolves the packed ctid (ADR Amendment A5, see
//! docs/at5-rowid-mechanism.md).
//!
//! Preflight-skip pattern (as in `test_at5_write_sqlitewasm` and
//! `cron_wasm_driver`): if the required wasm artifacts are missing OR the
//! `POSTGRES_TEST_URL` env var isn't set, the test SKIPS so a fresh
//! clone's `cargo test` still passes.
//!
//! `POSTGRES_TEST_URL` must be a DSN this backend can parse:
//!   * `postgres://user:pw@host:port/db` (URL form)
//!   * `host=.. port=.. user=.. password=.. database=..` (key=value form)
//! Auth: trust / cleartext / md5 (SCRAM is not supported). Plaintext only
//! (no TLS) — intended for a local test server.

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

/// Preflight: verify artifacts exist AND `POSTGRES_TEST_URL` is set. Returns
/// `Some((bin, dsn))` on success, None (test SKIPS) otherwise.
fn preflight(test_name: &str) -> Option<(PathBuf, String)> {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let postgres_ext = root.join("artifacts/extensions/postgreswasm.wasm");
    let core_wasm = root.join("target/wasm32-wasip2/release/ducklink_core.wasm");
    let cli_wasm = root.join("target/wasm32-wasip2/release/ducklink_cli.wasm");
    for required in [&bin, &postgres_ext, &core_wasm, &cli_wasm] {
        if !required.exists() {
            eprintln!(
                "{test_name}: skipping — missing {} \
                 (build with `make all && cargo component build \
                 -p postgreswasm-component --target wasm32-wasip2 --release && \
                 mkdir -p artifacts/extensions && cp \
                 target/wasm32-wasip1/release/postgreswasm.wasm \
                 artifacts/extensions/`)",
                required.display()
            );
            return None;
        }
    }
    let Ok(dsn) = std::env::var("POSTGRES_TEST_URL") else {
        eprintln!(
            "{test_name}: skipping — POSTGRES_TEST_URL not set (needs a \
             reachable postgres server with trust/cleartext/md5 auth)"
        );
        return None;
    };
    if dsn.trim().is_empty() {
        eprintln!("{test_name}: skipping — POSTGRES_TEST_URL is empty");
        return None;
    }
    Some((bin, dsn))
}

/// Build a distinct table name per test run so parallel + repeated runs don't
/// collide on the same server. Uses PID + a nanosecond timestamp.
fn unique_table(prefix: &str) -> String {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ducktest_{prefix}_{pid}_{now}")
}

/// Best-effort cleanup — DROP TABLE via the wasm CLI. Failures are silent
/// (the next unique-named table will be fresh anyway).
fn drop_table(bin: &Path, root: &Path, dsn: &str, table: &str) {
    let _ = run_ducklink(
        bin,
        root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 DROP TABLE IF EXISTS pg.{table};"
            ),
        ],
    );
}

#[test]
fn insert_into_attached_dispatches_to_storage_write() {
    let Some((bin, dsn)) = preflight("test_at5_write_postgreswasm::insert") else {
        return;
    };
    let root = repo_root();
    let table = unique_table("insert");

    // Pre-create the table (host-side CREATE TABLE on an attached foreign
    // catalog dispatches through storage-write-dispatch.create-table, which
    // this extension implements; run it via ducklink so the same code path
    // is exercised end-to-end).
    let create = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 CREATE TABLE pg.{table} (id INTEGER, name TEXT);"
            ),
        ],
    );
    assert!(
        create.status.success(),
        "{}",
        dump("CREATE TABLE failed", &create)
    );

    let ins = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 INSERT INTO pg.{table} (id, name) VALUES (100, 'new');"
            ),
        ],
    );
    if !ins.status.success() {
        drop_table(&bin, &root, &dsn, &table);
        panic!("{}", dump("INSERT failed", &ins));
    }

    // Verify the write is visible via a plain SELECT through the extension
    // (the mutation persisted on the postgres server -- no fs write-back
    // for a live-connection backend).
    let sel = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 SELECT id, name FROM pg.{table} WHERE id = 100;"
            ),
        ],
    );
    drop_table(&bin, &root, &dsn, &table);
    assert!(sel.status.success(), "{}", dump("SELECT failed", &sel));
    let stdout = String::from_utf8_lossy(&sel.stdout);
    assert!(
        stdout.contains("100") && stdout.contains("new"),
        "{}",
        dump("SELECT missing (100,'new') after INSERT", &sel)
    );
}

#[test]
fn update_attached_via_where_dispatches_after_rowid_prescan() {
    let Some((bin, dsn)) = preflight("test_at5_write_postgreswasm::update") else {
        return;
    };
    let root = repo_root();
    let table = unique_table("update");

    let create_and_insert = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 CREATE TABLE pg.{table} (id INTEGER, name TEXT); \
                 INSERT INTO pg.{table} (id, name) VALUES (100, 'new');"
            ),
        ],
    );
    assert!(
        create_and_insert.status.success(),
        "{}",
        dump("CREATE+INSERT failed", &create_and_insert)
    );

    let upd = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 UPDATE pg.{table} SET name = 'updated' WHERE id = 100;"
            ),
        ],
    );
    if !upd.status.success() {
        drop_table(&bin, &root, &dsn, &table);
        panic!("{}", dump("UPDATE failed", &upd));
    }

    let sel = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 SELECT name FROM pg.{table} WHERE id = 100;"
            ),
        ],
    );
    drop_table(&bin, &root, &dsn, &table);
    assert!(
        sel.status.success(),
        "{}",
        dump("SELECT after UPDATE failed", &sel)
    );
    let stdout = String::from_utf8_lossy(&sel.stdout);
    assert!(
        stdout.contains("updated"),
        "{}",
        dump("expected 'updated' in SELECT after UPDATE", &sel)
    );
}

#[test]
fn delete_attached_via_where_dispatches_after_rowid_prescan() {
    let Some((bin, dsn)) = preflight("test_at5_write_postgreswasm::delete") else {
        return;
    };
    let root = repo_root();
    let table = unique_table("delete");

    let create_and_insert = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 CREATE TABLE pg.{table} (id INTEGER, name TEXT); \
                 INSERT INTO pg.{table} (id, name) VALUES (100, 'new');"
            ),
        ],
    );
    assert!(
        create_and_insert.status.success(),
        "{}",
        dump("CREATE+INSERT failed", &create_and_insert)
    );

    let del = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 DELETE FROM pg.{table} WHERE id = 100;"
            ),
        ],
    );
    if !del.status.success() {
        drop_table(&bin, &root, &dsn, &table);
        panic!("{}", dump("DELETE failed", &del));
    }

    let cnt = run_ducklink(
        &bin,
        &root,
        &[
            "sql",
            &format!(
                "LOAD postgreswasm; \
                 ATTACH '{dsn}' AS pg (TYPE postgreswasm); \
                 SELECT COUNT(*) FROM pg.{table} WHERE id = 100;"
            ),
        ],
    );
    drop_table(&bin, &root, &dsn, &table);
    assert!(
        cnt.status.success(),
        "{}",
        dump("SELECT COUNT after DELETE failed", &cnt)
    );
    let stdout = String::from_utf8_lossy(&cnt.stdout);
    // The count row must be 0. Look for "| 0 |" (box output) or a bare "0"
    // line (csv). Rejecting substring "100" is too loose since the DSN may
    // contain digits -- we key on the count column shape.
    let has_zero_count = stdout.lines().any(|line| {
        let t = line.trim();
        t == "0"
            || t.starts_with("| 0")
            || (t.starts_with('|') && t.trim_matches('|').trim() == "0")
    });
    assert!(
        has_zero_count,
        "{}",
        dump("expected COUNT(*) = 0 after DELETE", &cnt)
    );
}
