//! End-to-end lock-in for `ducklink cron run --wasm-driver`.
//!
//! `cron-driver-tool.wasm` is a `wasi:cli/run` component (not a
//! `duckdb:extension`), so it doesn't fit the standard `tooling/smoke.py`
//! extension harness. Rather than shelling out to `extensions/cron-driver-tool/
//! smoke.sh` from CI, this test drives the same flow directly through
//! `target/release/ducklink` so a regression trips `cargo test` on the host
//! crate.
//!
//! Because a `* * * * *` schedule needs a minute boundary to become due, the
//! test sleeps ~65 seconds. It is marked `#[ignore]` so `cargo test` skips
//! it by default; run with:
//!
//!   cargo test -p ducklink-host --test cron_wasm_driver -- --ignored --nocapture
//!
//! The test also preflight-checks the required build artifacts and skips
//! cleanly (returning success) if any are missing — otherwise a fresh
//! checkout without `make all && make cron-driver` would break `cargo test
//! -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

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

/// Render an Output for `assert!` panics — the wasm-driver path emits a lot
/// of `[dotcmd loaded ...]` noise on stderr, so a bare `assert!` failure
/// message is useless without the captured streams.
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
#[ignore = "long: sleeps ~65s to cross a cron minute boundary"]
fn cron_run_wasm_driver_fires_scheduled_job() {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let cron_driver_tool = root.join("artifacts/extensions/cron_driver_tool.wasm");
    let cron_ext = root.join("artifacts/extensions/cron.wasm");
    let cron_scheduler_ext = root.join("artifacts/extensions/cron_scheduler.wasm");

    // Preflight — skip cleanly if the repo isn't built. `cargo test
    // -- --ignored` on a fresh clone would otherwise be a hard failure.
    for required in [&bin, &cron_driver_tool, &cron_ext, &cron_scheduler_ext] {
        if !required.exists() {
            eprintln!(
                "cron_wasm_driver: skipping — missing {} (build with `make all && make cron-driver`)",
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
    let workdir_rel = workdir
        .path()
        .strip_prefix(&root)
        .expect("tempdir is under repo root");
    let db_rel = workdir_rel.join("cron.duckdb");
    let db_str = db_rel.to_str().expect("utf-8 tmp path");

    // 1. init
    let init = run_ducklink(&bin, &root, &["cron", "init", "--db", db_str]);
    assert!(init.status.success(), "{}", dump("cron init failed", &init));

    // 2. schedule a job that fires every minute
    let schedule = run_ducklink(
        &bin,
        &root,
        &[
            "cron",
            "schedule",
            "--db",
            db_str,
            "demo",
            "* * * * *",
            "CHECKPOINT",
        ],
    );
    assert!(
        schedule.status.success(),
        "{}",
        dump("cron schedule failed", &schedule)
    );

    // 3. sleep across a minute boundary so cron_due() returns the job
    std::thread::sleep(Duration::from_secs(65));

    // 4. run once through the wasm driver — the payload of the whole test
    let run = run_ducklink(
        &bin,
        &root,
        &["cron", "run", "--db", db_str, "--wasm-driver", "--once"],
    );
    assert!(
        run.status.success(),
        "{}",
        dump("cron run --wasm-driver --once failed", &run)
    );
    assert!(
        contains(&run.stderr, "fired 1 job(s)"),
        "{}",
        dump("expected 'fired 1 job(s)' on stderr", &run)
    );

    // 5. cron list — parse loose (stdout is prompt-interleaved) for the
    //    demo row + its updated status. Substring matching is fine here;
    //    a full JSON parse would need the same prompt-stripping helper
    //    driver_exec.rs::parse_csv_rows uses.
    let list = run_ducklink(&bin, &root, &["cron", "list", "--db", db_str]);
    assert!(list.status.success(), "{}", dump("cron list failed", &list));
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("\"name\":\"demo\""),
        "{}",
        dump("cron list missing demo row", &list)
    );
    assert!(
        list_stdout.contains("\"last_status\":\"fired\""),
        "{}",
        dump("cron list did not record last_status=fired", &list)
    );
}
