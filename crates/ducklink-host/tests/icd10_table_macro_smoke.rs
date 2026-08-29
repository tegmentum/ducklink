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

/// Sentinel-string pattern: each macro query returns a single `status`
/// row that resolves to `<TAG>_OK` when the macro produced the expected
/// shape and content, or `<TAG>_MISSING` otherwise. String matching on
/// the sentinel is bulletproof against DuckDB CLI box-drawing quirks and
/// distinguishes "bound but empty" from "returns real rows" — a bare
/// `COUNT(*) AS n` assertion passes on both.
fn assert_ok(stdout: &str, ok_tag: &str) {
    assert!(
        stdout.contains(ok_tag),
        "expected `{ok_tag}` in stdout, got:\n{stdout}"
    );
}

#[test]
fn icd10_ancestors_table_macro_end_to_end() -> anyhow::Result<()> {
    // E11 (block leaf), E08-E13 (block), 4 (chapter root) are all real
    // ancestors of E11.9 in ICD-10-CM's parent_code hierarchy.
    let stdout = run_icd10_variant(
        "cm",
        "SELECT CASE \
            WHEN COUNT(*) FILTER (WHERE code = 'E11') > 0 \
             AND COUNT(*) FILTER (WHERE code = 'E08-E13') > 0 \
            THEN 'ANCESTORS_OK' ELSE 'ANCESTORS_MISSING' END AS status \
         FROM icd10.ancestors('E11.9');",
    )?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert_ok(&stdout, "ANCESTORS_OK");
    Ok(())
}

#[test]
fn icd10_descendants_table_macro_end_to_end() -> anyhow::Result<()> {
    // E11.0 (Type 2 with hyperosmolarity) and E11.9 (Type 2 without
    // complications) are stable direct children of E11 in ICD-10-CM.
    let stdout = run_icd10_variant(
        "cm",
        "SELECT CASE \
            WHEN COUNT(*) FILTER (WHERE code = 'E11.0') > 0 \
             AND COUNT(*) FILTER (WHERE code = 'E11.9') > 0 \
            THEN 'DESCENDANTS_OK' ELSE 'DESCENDANTS_MISSING' END AS status \
         FROM icd10.descendants('E11');",
    )?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert_ok(&stdout, "DESCENDANTS_OK");
    Ok(())
}

#[test]
fn icd10_pcs_axes_table_macro_end_to_end() -> anyhow::Result<()> {
    // Self-select any real PCS code (decouples from PCS-year drift), assert
    // the macro returns exactly the one row the pcs_axes PK guarantees and
    // that every axis label came through non-null.
    let stdout = run_icd10_variant(
        "pcs",
        "SELECT CASE \
            WHEN COUNT(*) = 1 \
             AND MIN(section) IS NOT NULL \
             AND MIN(body_system) IS NOT NULL \
             AND MIN(root_operation) IS NOT NULL \
            THEN 'AXES_OK' ELSE 'AXES_MISSING' END AS status \
         FROM icd10.pcs_axes((SELECT code FROM main.pcs_axes LIMIT 1));",
    )?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert_ok(&stdout, "AXES_OK");
    Ok(())
}

#[test]
fn icd10_who_xrefs_table_macro_end_to_end() -> anyhow::Result<()> {
    // Self-select a code that actually has xrefs, then assert the macro
    // walked UNNEST + JOIN concepts and returned at least one concept-shaped
    // partner row with a populated display.
    let stdout = run_icd10_variant(
        "who",
        "SELECT CASE \
            WHEN COUNT(*) > 0 AND MIN(display) IS NOT NULL \
            THEN 'XREFS_OK' ELSE 'XREFS_MISSING' END AS status \
         FROM icd10.who_xrefs(( \
            SELECT code FROM main.who_metadata WHERE xref_codes IS NOT NULL LIMIT 1));",
    )?;
    if stdout.is_empty() {
        return Ok(());
    }
    assert_ok(&stdout, "XREFS_OK");
    Ok(())
}
