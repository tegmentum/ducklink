//! End-to-end smoke test for the table-macro forwarding chain
//! (ducklink WIT + host handoff -> duckdb-wasm core `register_pending_table_macro`
//! -> icd-10-ducklink-component's `icd10.ancestors` table macro).
//!
//! Loads the icd-10-ducklink-component wasm extension
//! (`artifacts/extensions/icd10_ducklink.wasm`, staged by the smoke-test setup
//! from `~/git/icd-10-ducklink-component/target/wasm32-wasip1/release/
//! icd_10_ducklink_component.wasm`) against a real ICD-10-CM `.duckdb`
//! artifact built by the sibling `icd-10` CLI
//! (`~/icd-10/artifacts/current-cm/icd10.duckdb`) through `CliHarness`
//! (the same harness `load_sample_extension_component`'s neighbors use),
//! and runs `SELECT * FROM icd10.ancestors('E11.9')` — proving the whole
//! chain end-to-end: WIT table-macro-registration record -> host
//! `register_pending_table_macro` -> `CREATE MACRO icd10.ancestors(code) AS
//! TABLE (...)` -> a real query against it.
//!
//! Skips (rather than fails) when either input artifact isn't present on
//! disk, since both are built out-of-band (see the doc comments above) and
//! this test's job is to prove the wiring once those artifacts exist, not to
//! build them.

use std::path::{Path, PathBuf};

use ducklink_host::CliHarness;

fn icd10_duckdb_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join("icd-10/artifacts/current-cm/icd10.duckdb");
    path.exists().then_some(path)
}

fn icd10_extension_wasm_path() -> Option<PathBuf> {
    let path = ducklink_host::workspace_root().join("artifacts/extensions/icd10_ducklink.wasm");
    path.exists().then_some(path)
}

#[test]
fn icd10_ancestors_table_macro_end_to_end() -> anyhow::Result<()> {
    let Some(db_host_path) = icd10_duckdb_path() else {
        eprintln!("skipping: no icd10.duckdb at ~/icd-10/artifacts/current-cm/icd10.duckdb");
        return Ok(());
    };
    let Some(_ext) = icd10_extension_wasm_path() else {
        eprintln!(
            "skipping: no staged icd10_ducklink.wasm at artifacts/extensions/icd10_ducklink.wasm"
        );
        return Ok(());
    };

    let db_dir = db_host_path
        .parent()
        .expect("icd10.duckdb has a parent directory");
    let db_guest_path = "icd10.duckdb";
    let preopens = [(db_dir, ".")];

    let args = [
        "duckdb-cli",
        db_guest_path,
        "--load-extension",
        "icd10_ducklink",
        "-c",
        "SELECT * FROM icd10.ancestors('E11.9') LIMIT 20;",
    ];

    let mut harness = CliHarness::new(&args, &preopens)?;
    let status = harness.run()?;
    let stdout = harness.stdout().unwrap_or_default();
    let stderr = harness.stderr().unwrap_or_default();

    if status.is_err() {
        panic!(
            "icd10.ancestors('E11.9') query failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    println!("=== icd10.ancestors('E11.9') stdout ===\n{stdout}");
    if !stderr.trim().is_empty() {
        println!("=== stderr ===\n{stderr}");
    }

    assert!(
        stdout.contains("E11") || stdout.contains("code"),
        "expected ancestor rows (e.g. E11) in stdout, got:\n{stdout}"
    );

    Ok(())
}
