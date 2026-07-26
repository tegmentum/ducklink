//! End-to-end lock-in for the native `ducklink cron run` per-job failure
//! capture.
//!
//! Before the driver refactor, `cron run` (native path) shelled out to
//! `run_cli_capture` per SQL and swallowed the CLI's stderr. Every fired job
//! — succeeded or failed — landed with `last_status = 'fired'` and
//! `last_error = ''`. A silently-failing job looked identical to a healthy
//! one in `cron list`.
//!
//! After the refactor: every SQL runs through
//! `driver_core_exec(&mut state, sql) -> Result<u64, String>`, and the
//! `Err(String)` carries the DuckDB error text verbatim. `tick()` records
//! `last_status = 'failed'` + `last_error = <that text>` on failure and
//! `last_status = 'fired'` only on success. This test proves that.
//!
//! `cron_wasm_driver.rs` covers the happy path through `--wasm-driver` and
//! waits ~65s to cross a real minute boundary. This test targets the NATIVE
//! path (no `--wasm-driver`) and sidesteps the wall-clock wait by manually
//! setting `next_run_at = 0` on both jobs so `cron_due()` returns them
//! immediately. It's still `#[ignore]`d because the CLI cold-loads the wasm
//! core (~7s on a cold compile cache), which we don't want in the default
//! `cargo test` run.
//!
//! Run with:
//!
//!   cargo test -p ducklink-host --test cron_native_failure -- --ignored --nocapture

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

/// Run `ducklink <args...>` with cwd set to the repo root so the CLI's default
/// `artifacts/extensions/` resolution finds the built components.
fn run_ducklink(bin: &Path, root: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("spawn ducklink {args:?}: {e}"))
}

/// Render an Output for `assert!` panics — the native cron path emits a lot
/// of `[dotcmd loaded ...]`, `[extension-manager] ...`, and `[wasi-fs] ...`
/// noise on stderr, so a bare `assert!` failure message is useless without
/// the captured streams.
fn dump(label: &str, out: &Output) -> String {
    format!(
        "{label}\n  status: {}\n  stdout:\n{}\n  stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(haystack).contains(needle)
}

#[test]
#[ignore = "slow: cold-loads the wasm core each subprocess (~7s per invocation)"]
fn cron_run_native_records_per_job_failure() {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let cron_ext = root.join("artifacts/extensions/cron.wasm");
    let cron_scheduler_ext = root.join("artifacts/extensions/cron_scheduler.wasm");

    // Preflight — skip cleanly if the repo isn't built. `cargo test
    // -- --ignored` on a fresh clone would otherwise be a hard failure.
    // (This is the NATIVE path, so `cron_driver_tool.wasm` is NOT required.)
    for required in [&bin, &cron_ext, &cron_scheduler_ext] {
        if !required.exists() {
            eprintln!(
                "cron_native_failure: skipping — missing {} (build with `make all`)",
                required.display()
            );
            return;
        }
    }

    // The wasi-fs shim treats the leading `/` on an absolute path as a
    // relative segment under a preopen, so `/foo/bar` opens against
    // `<preopen>/foo/bar` — mangled and silently unwritable. Pass the DB
    // path RELATIVE to cwd (=repo root) so the shim's resolution matches
    // the host's. The tempdir goes under `target/` so it's covered by the
    // cwd preopen and gets cleaned up on drop.
    let target = root.join("target");
    std::fs::create_dir_all(&target).expect("create target dir");
    let workdir = tempfile::tempdir_in(&target).expect("tempdir under target");
    let workdir_rel = workdir.path().strip_prefix(&root).expect("tempdir is under repo root");
    let db_rel = workdir_rel.join("cron.duckdb");
    let db_str = db_rel.to_str().expect("utf-8 tmp path");

    // 1. init
    let init = run_ducklink(&bin, &root, &["cron", "init", "--db", db_str]);
    assert!(init.status.success(), "{}", dump("cron init failed", &init));

    // 2a. schedule the good job — CHECKPOINT always succeeds on an open DB.
    let good = run_ducklink(
        &bin,
        &root,
        &[
            "cron",
            "schedule",
            "--db",
            db_str,
            "good",
            "* * * * *",
            "CHECKPOINT",
        ],
    );
    assert!(
        good.status.success(),
        "{}",
        dump("cron schedule good failed", &good)
    );

    // 2b. schedule the bad job — SELECT on a missing table fails with a
    //     Catalog Error at plan time. The verbatim error text is stable
    //     across DuckDB releases: "Catalog Error: Table with name
    //     missing_table does not exist!".
    let bad = run_ducklink(
        &bin,
        &root,
        &[
            "cron",
            "schedule",
            "--db",
            db_str,
            "bad",
            "* * * * *",
            "SELECT * FROM missing_table",
        ],
    );
    assert!(
        bad.status.success(),
        "{}",
        dump("cron schedule bad failed", &bad)
    );

    // 3. Force-fire without waiting: manually stamp both jobs'
    //    `next_run_at = 0` via the DuckDB CLI. `cron_due(now)` selects
    //    rows where `next_run_at <= now`, so 0 makes them immediately due.
    //    We LOAD the two extensions defensively even though `__cron_jobs`
    //    is a plain table — the extensions register other objects the
    //    catalog wants resident. `ducklink -- <db> -c "SQL"` forwards
    //    the args after `--` to the wasm DuckDB CLI verbatim.
    let force_due = run_ducklink(
        &bin,
        &root,
        &[
            "--",
            db_str,
            "-c",
            "LOAD cron; LOAD cron_scheduler; UPDATE __cron_jobs SET next_run_at = 0;",
        ],
    );
    assert!(
        force_due.status.success(),
        "{}",
        dump("force-due UPDATE failed", &force_due)
    );

    // 4. Run one tick through the NATIVE driver (no --wasm-driver).
    //    This is the payload of the whole test.
    let run = run_ducklink(&bin, &root, &["cron", "run", "--db", db_str, "--once"]);
    assert!(
        run.status.success(),
        "{}",
        dump("cron run --once failed", &run)
    );
    assert!(
        contains(&run.stderr, "fired 2 job(s)"),
        "{}",
        dump("expected 'fired 2 job(s)' on stderr", &run)
    );

    // 5. cron list — parse loose (stdout is prompt-interleaved) for the
    //    two rows and their per-job status/error. Substring matching is
    //    fine here; a full JSON parse would need the same prompt-stripping
    //    helper driver_exec.rs::parse_csv_rows uses.
    let list = run_ducklink(&bin, &root, &["cron", "list", "--db", db_str]);
    assert!(list.status.success(), "{}", dump("cron list failed", &list));
    let list_stdout = String::from_utf8_lossy(&list.stdout);

    // The two rows are separate JSON objects on stdout. Substring is
    // enough — an incorrect implementation before the refactor would
    // record `last_status:"fired"` for BOTH jobs, so any presence of
    // `last_status:"failed"` proves the refactor's per-job err_lit path
    // is live.
    assert!(
        list_stdout.contains("\"name\":\"good\""),
        "{}",
        dump("cron list missing good row", &list)
    );
    assert!(
        list_stdout.contains("\"name\":\"bad\""),
        "{}",
        dump("cron list missing bad row", &list)
    );
    assert!(
        list_stdout.contains("\"last_status\":\"fired\""),
        "{}",
        dump("cron list missing last_status=fired (good job)", &list)
    );
    assert!(
        list_stdout.contains("\"last_status\":\"failed\""),
        "{}",
        dump("cron list missing last_status=failed (bad job)", &list)
    );
    // The verbatim DuckDB error text — this is the exact regression the
    // refactor fixed. Prefix match: the DuckDB error line may continue
    // with a hint about candidate table names.
    assert!(
        list_stdout
            .contains("\"last_error\":\"Catalog Error: Table with name missing_table does not exist!"),
        "{}",
        dump("cron list missing verbatim Catalog Error text", &list)
    );
}
