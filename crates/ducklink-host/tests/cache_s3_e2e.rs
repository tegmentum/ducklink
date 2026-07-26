//! End-to-end coverage for the wasm cache-component's s3:// backend
//! (`extensions/cache-component/src/lib.rs::resolve_s3`, wired via
//! `wac plug component:s3-wasm` in commit d2b8870).
//!
//! The http/https backend is already exercised end-to-end by
//! `cache_concurrent_miss.rs` (against a local Python HTTP server).
//! The s3 backend is a different transport path: it rides the
//! s3-wasm component's `s3-base.get-object` export, which internally
//! constructs its own `wasi:http/outgoing-handler` request. This test
//! proves the wac composition survives round-tripping through
//! `SELECT cache(cfg, 's3://...')` and produces a content-addressed
//! blob under `<cache_root>/objects/<hh>/<rest>` matching what the
//! http path produces.
//!
//! ## Multi-process, not multi-thread
//! Same reasoning as cache_concurrent_miss.rs. The interesting
//! locking behaviour is between separate `ducklink` OS processes.
//!
//! ## Network dependency
//! By default the script hits `s3://noaa-goes16/index.html` — an
//! anonymous public object in the NOAA GOES-16 AWS Open Data bucket.
//! CI without egress to `s3.amazonaws.com` should either skip this
//! test (leave `--ignored` on) or override the URI to point at a
//! reachable fixture (`S3_URI=s3://...` env var passed to the
//! script).
//!
//! ## Run
//! Marked `#[ignore]` so `cargo test` skips it by default (heavy
//! prereqs + external network). Trigger with:
//!
//! ```text
//!   cargo test -p ducklink-host --test cache_s3_e2e \
//!     -- --ignored --nocapture
//! ```
//!
//! Preflight-skips when the required build artifacts are missing
//! (fresh clone without `make host && make cache`) — matches the
//! shape of `cron_wasm_driver.rs` / `cache_concurrent_miss.rs`.
//!
//! The server-less orchestration + assertion set lives in
//! `scripts/cache-s3-anonymous.sh` so exactly one artifact carries
//! the test truth. This file drives that script.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

#[test]
#[ignore = "spawns real ducklink processes + real S3 traffic; run with --ignored"]
fn cache_resolves_anonymous_s3_via_s3_wasm() {
    let root = repo_root();
    let bin = root.join("target/release/ducklink");
    let cache_wasm = root.join("artifacts/extensions/cache.wasm");
    let script = root.join("scripts/cache-s3-anonymous.sh");

    // Preflight: a fresh checkout without `make host && make cache` should
    // skip cleanly rather than hard-fail `cargo test -- --ignored`.
    for required in [&bin, &cache_wasm, &script] {
        if !required.exists() {
            eprintln!(
                "cache_s3_e2e: skipping — missing {} \
                 (build with `make host && make cache`)",
                required.display()
            );
            return;
        }
    }

    // The script itself does the s3-wasm-composed check (via wasm-tools
    // if available); no need to repeat it here. Match the concurrent-miss
    // test's `N` override plumbing to keep the two consistent.
    let out = Command::new("bash")
        .arg(&script)
        // Small worker count — this hits real AWS. cache-s3-anonymous.sh
        // defaults to 2; we set it explicitly for reproducibility.
        .env("N", "2")
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", script.display()));

    if !out.status.success() {
        panic!(
            "cache-s3-anonymous script failed with status {}\n\
             stdout:\n{}\n\
             stderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("cache-s3-anonymous: PASS"),
        "script exited 0 but did not print PASS marker; output:\n{combined}"
    );
    assert!(
        combined.contains("round 2 returned same URI"),
        "script exited 0 but did not confirm the second-invocation cache hit; \
         output:\n{combined}"
    );
}
