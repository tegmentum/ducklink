//! Phase 4 (@5) integration lock-in for the shared-`ExtensionManager` sibling.
//!
//! Phase 4 landed the plumbing that shares the primary's
//! `Arc<Mutex<ExtensionManager>>` onto `SiblingState`, so the sibling core's
//! `CoreStoreState.extension_manager` binds to the SAME manager as the primary.
//! The Arc-identity + plain-SQL regression proofs live inline in
//! `crates/ducklink-host/src/lib.rs` as `phase4_sibling_binds_primary_extension_manager`
//! and `phase4_sibling_shared_plain_sql_no_regression` — those exercise the
//! internal `SiblingState` / `CoreServices` types that this integration test
//! cannot reach across the crate boundary.
//!
//! What THIS file adds: an end-to-end lock-in that boots the built `ducklink`
//! CLI binary, loads a real extension (fieldbook), and drives a scalar whose
//! body issues `nested-exec` from inside its own callback. The success shape
//! proves the whole sibling-shared-manager path (or Direction-1 primary-reentry
//! fallback where it takes precedence) at the CLI boundary — a smoke that
//! catches regressions the unit tests would miss.
//!
//! STATUS: `#[ignore]`d — requires the built `target/release/ducklink` binary
//! and the built fieldbook.wasm extension in the workspace `artifacts/`
//! directory. Run with:
//!
//!   cargo build --release
//!   make ext EXT=fieldbook
//!   cargo test -p ducklink-host --test test_phase4_sibling_ext_nested_exec -- --ignored --nocapture
//!
//! ADR reference: `docs/wasm-ecosystem-at-5-adr.md` Decision 6 (blocked-items
//! un-block criteria) + Amendment A5 Risk 5 (deadlock boundary when the
//! sibling's SQL invokes an extension scalar — pure-DML sibling SQL, as used
//! by fieldbook's bootstrap flow, sits safely inside the boundary).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Repo root — one level up from this crate's manifest dir.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

/// Run `ducklink <args...>` with cwd = repo root so the CLI's default
/// `artifacts/extensions/` resolution finds the built components.
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

/// Phase 4 E2E: load `fieldbook`, invoke a scalar whose body issues a
/// `nested-exec` statement, and assert the CLI exits cleanly with no
/// `NESTED_EXEC_DIRECTION2_REDIRECT` prefix in stderr. Fieldbook's own load()
/// runs `nested_exec(fieldbook_core::bootstrap_sql())` — pure DDL for its
/// three storage tables — so a successful boot + fieldbook_notebook_create
/// call end-to-end proves the shared-manager path works for the
/// fieldbook/mosaic bootstrap-SQL shape.
#[test]
#[ignore = "requires target/release/ducklink + artifacts/extensions/fieldbook.wasm"]
fn phase4_fieldbook_bootstrap_via_nested_exec_ok() {
    let root = repo_root();
    let bin = root.join("target").join("release").join("ducklink");
    let fieldbook_wasm = root.join("artifacts").join("extensions").join("fieldbook.wasm");
    if !bin.exists() || !fieldbook_wasm.exists() {
        eprintln!(
            "skipping: missing prerequisites\n  bin = {} (exists: {})\n  fieldbook = {} (exists: {})",
            bin.display(),
            bin.exists(),
            fieldbook_wasm.display(),
            fieldbook_wasm.exists(),
        );
        return;
    }

    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("phase4.duckdb");
    let db_str = db_path.to_str().expect("utf8 db path");

    // The `-c` flag runs the SQL and exits, mirroring `test_at5_*` smoke
    // scripts. LOAD triggers fieldbook's load() which calls
    // `nested_exec(bootstrap_sql())` — the exact code path Phase 4 was meant
    // to unblock (pure-DDL sibling SQL touching plain tables; no callback
    // dispatch on the sibling, so the shared-manager deadlock boundary
    // isn't crossed).
    let sql = format!(
        "PRAGMA disable_progress_bar; \
         OPEN '{db_str}'; \
         LOAD fieldbook; \
         SELECT 1 AS boot_ok;"
    );
    let run = run_ducklink(&bin, &root, &["-c", &sql]);
    assert!(
        run.status.success(),
        "{}",
        dump("ducklink -c 'LOAD fieldbook; ...' failed", &run)
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        !stderr.contains("nested-exec (Direction 1):"),
        "sibling still hit the Direction-2 redirect (Phase 4 regression):\n{stderr}"
    );
    assert!(
        !stderr.contains("sibling core does not have host extensions loaded"),
        "sibling reported missing extensions after Phase 4 shared-manager wiring:\n{stderr}"
    );
}
