//! End-to-end lock-in for the wasm cache-component's file-lock wiring
//! (`extensions/cache-component/src/lib.rs::resolver`, commit e84d8f9).
//!
//! N processes race calling `cache('http://127.0.0.1:PORT/payload')` on a
//! not-yet-cached URL. With the file-lock in place the winner performs the
//! single HTTP GET, publishes the blob + catalog row, and the losers observe
//! the winner's entry on the re-lookup-under-lock branch and return the SAME
//! `file://` URI without touching the network. Without the file-lock every
//! racing peer fires its own GET and the server sees N requests.
//!
//! The runtime already unit-tests the underlying primitive
//! (`ducklink_runtime::extension::tests::file_lock_race_between_threads_serializes`);
//! THIS test proves the WASM CACHE COMPONENT actually calls into it, i.e.
//! that the WIT import at `extensions/cache-component/wit/file-lock.wit`
//! resolves to the runtime's `impl extension_file_lock::Host` at instantiate
//! time and that `resolver()` actually blocks in `acquire-exclusive` on
//! contention. Because the wasm world composition and the host-side wiring
//! are the two things a plain unit test can't cover, we drive real
//! `ducklink` child processes end-to-end.
//!
//! ## Multi-process, not multi-thread
//! On macOS/Linux `fs2::FileExt::lock_exclusive` is a `flock(2)` on the fd,
//! which is per-open-file-description. Multiple threads in ONE process each
//! opening the lock file would serialise via the same mechanism, but the
//! wasm cache's motivating scenario in the module doc is
//! multi-PROCESS (N `ducklink` invocations sharing an on-disk cache root).
//! We test what the memo describes.
//!
//! ## Run
//! Marked `#[ignore]` so `cargo test` skips it by default (the shell
//! script + wasm artifact are heavy prereqs). Trigger with:
//!
//! ```text
//!   cargo test -p ducklink-host --test cache_concurrent_miss \
//!     -- --ignored --nocapture
//! ```
//!
//! Preflight-skip (returns success without asserting) when the required
//! build artifacts are missing, matching the pattern in
//! `cron_wasm_driver.rs` — a fresh clone shouldn't hard-fail
//! `cargo test -- --ignored`.
//!
//! The actual server + N-worker orchestration + assertion set lives in
//! `scripts/cache-concurrent-miss.sh` so exactly one artifact carries the
//! test truth. This test file drives that script.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

#[test]
#[ignore = "spawns real ducklink processes + local HTTP server; run with --ignored"]
fn cache_serialises_concurrent_misses_via_file_lock() {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let cache_wasm = root.join("artifacts/extensions/cache.wasm");
    let script = root.join("scripts/cache-concurrent-miss.sh");

    // Preflight — a fresh checkout without `make host && make cache` skips
    // rather than failing. The script itself will also fail fast with a
    // clear message if these vanish between here and there.
    for required in [&bin, &cache_wasm, &script] {
        if !required.exists() {
            eprintln!(
                "cache_concurrent_miss: skipping — missing {} \
                 (build with `make host && make cache`)",
                required.display()
            );
            return;
        }
    }

    // The composed cache.wasm at HEAD (73021f2) predates the file-lock
    // wiring in e84d8f9. If wasm-tools is available, guard against running
    // a wasm without the file-lock import — the assertions inside the
    // script would fail anyway, but a targeted panic gives a clearer
    // diagnostic. Skip the check silently if wasm-tools isn't installed.
    if let Some(wit) = wit_dump(&cache_wasm) {
        assert!(
            wit.contains("file-lock"),
            "cache.wasm at {} does not import duckdb:extension/file-lock; \
             rebuild after commit e84d8f9 via `make cache`",
            cache_wasm.display()
        );
    }

    let out = Command::new("bash")
        .arg(&script)
        .env("N", "6")
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", script.display()));

    if !out.status.success() {
        panic!(
            "cache-concurrent-miss script failed with status {}\n\
             stdout:\n{}\n\
             stderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    // Belt-and-suspenders: the script prints a PASS line + a `gets=N`
    // token. Fail if we don't see BOTH the PASS marker AND `gets=1`
    // (protects against a future script edit that accidentally weakens
    // the assertion to "at least one GET").
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("cache-concurrent-miss: PASS"),
        "script exited 0 but did not print PASS marker; output:\n{combined}"
    );
    assert!(
        combined.contains("gets=1"),
        "script exited 0 but did not report exactly one GET; output:\n{combined}"
    );
}

/// Ask `wasm-tools component wit` for the composed component's WIT dump; None
/// if wasm-tools isn't on PATH (in which case the guard is skipped).
fn wit_dump(cache_wasm: &Path) -> Option<String> {
    let out = Command::new("wasm-tools")
        .args(["component", "wit"])
        .arg(cache_wasm)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}
