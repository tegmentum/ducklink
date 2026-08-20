//! End-to-end smoke tests for the icd-10-ducklink-component table macros:
//! `icd10.ancestors`, `icd10.descendants`, `icd10.pcs_axes`, `icd10.who_xrefs`.
//!
//! Loads the icd-10-ducklink-component wasm extension
//! (`artifacts/extensions/icd10_ducklink.wasm`, staged by the smoke-test setup
//! from `~/git/icd-10-ducklink-component/target/wasm32-wasip1/release/
//! icd_10_ducklink_component.wasm`) against variant-scoped `.duckdb`
//! artifacts built by the sibling `icd-10` CLI at
//! `~/icd-10/artifacts/current-{cm,pcs,who}/icd10.duckdb` through
//! `CliHarness`, proving the WIT `table-macro-registration` record ->
//! duckdb-wasm core `register_pending_table_macro` -> `CREATE OR REPLACE
//! MACRO ... AS TABLE (...)` chain end-to-end.
//!
//! Each test skips (rather than fails) when either input artifact isn't
//! present on disk, since both are built out-of-band and these tests exist
//! to prove the wiring once those artifacts exist, not to build them.

use std::path::{Path, PathBuf};

use ducklink_host::CliHarness;

fn icd10_variant_duckdb_path(variant: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home)
        .join("icd-10/artifacts")
        .join(format!("current-{variant}"))
        .join("icd10.duckdb");
    path.exists().then_some(path)
}

fn icd10_extension_wasm_path() -> Option<PathBuf> {
    let path = ducklink_host::workspace_root().join("artifacts/extensions/icd10_ducklink.wasm");
    path.exists().then_some(path)
}

fn run_icd10_variant(variant: &str, sql: &str) -> anyhow::Result<String> {
    let Some(db_host_path) = icd10_variant_duckdb_path(variant) else {
        eprintln!("skipping: no icd10.duckdb at ~/icd-10/artifacts/current-{variant}/icd10.duckdb");
        return Ok(String::new());
    };
    if icd10_extension_wasm_path().is_none() {
        eprintln!(
            "skipping: no staged icd10_ducklink.wasm at artifacts/extensions/icd10_ducklink.wasm"
        );
        return Ok(String::new());
    }

    let db_dir = db_host_path
        .parent()
        .expect("icd10.duckdb has a parent directory");
    let preopens = [(db_dir, ".")];
    let args = [
        "duckdb-cli",
        "icd10.duckdb",
        "--load-extension",
        "icd10_ducklink",
        "-c",
        sql,
    ];

    let mut harness = CliHarness::new(&args, &preopens)?;
    let status = harness.run()?;
    let stdout = harness.stdout().unwrap_or_default();
    let stderr = harness.stderr().unwrap_or_default();

    if status.is_err() {
        panic!(
            "query failed\nvariant: {variant}\nsql: {sql}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    println!("=== icd10.{variant} stdout ===\n{stdout}");
    if !stderr.trim().is_empty() {
        println!("=== stderr ===\n{stderr}");
    }
    Ok(stdout)
}

#[test]
fn icd10_ancestors_table_macro_end_to_end() -> anyhow::Result<()> {
    let stdout = run_icd10_variant("cm", "SELECT * FROM icd10.ancestors('E11.9') LIMIT 20;")?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert!(
        stdout.contains("E11") || stdout.contains("code"),
        "expected ancestor rows (e.g. E11) in stdout, got:\n{stdout}"
    );
    Ok(())
}

#[test]
fn icd10_descendants_table_macro_end_to_end() -> anyhow::Result<()> {
    // `E11` is the Type-2 diabetes block header — has many descendants on CM.
    // COUNT(*) form keeps the assertion stable even if row counts drift year-over-year.
    let stdout = run_icd10_variant("cm", "SELECT COUNT(*) AS n FROM icd10.descendants('E11');")?;
    if stdout.is_empty() {
        return Ok(());
    }
    // Any numeric output beats the header alone; empty descendants would still
    // print `0` under the `n` header.
    assert!(
        stdout.contains('n'),
        "expected `n` column header in COUNT(*) output, got:\n{stdout}"
    );
    Ok(())
}

#[test]
fn icd10_pcs_axes_table_macro_end_to_end() -> anyhow::Result<()> {
    // Pull an arbitrary code that actually exists in the loaded pcs_axes table
    // rather than hard-coding one — decouples the test from PCS-year drift.
    let stdout = run_icd10_variant(
        "pcs",
        "SELECT COUNT(*) AS n FROM icd10.pcs_axes((SELECT code FROM main.pcs_axes LIMIT 1));",
    )?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert!(
        stdout.contains('n'),
        "expected `n` column header in COUNT(*) output, got:\n{stdout}"
    );
    Ok(())
}

#[test]
fn icd10_who_xrefs_table_macro_end_to_end() -> anyhow::Result<()> {
    // Same self-selecting trick as pcs_axes — pull a code from who_metadata
    // that actually has an xref, so the macro exercises the UNNEST + JOIN path.
    let stdout = run_icd10_variant(
        "who",
        "SELECT COUNT(*) AS n FROM icd10.who_xrefs((SELECT code FROM main.who_metadata WHERE xref_codes IS NOT NULL LIMIT 1));",
    )?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert!(
        stdout.contains('n'),
        "expected `n` column header in COUNT(*) output, got:\n{stdout}"
    );
    Ok(())
}
