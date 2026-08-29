//! End-to-end smoke test for the snomed-ct-ducklink-component
//! `snomed.subsumed_by` table macro.
//!
//! Loads `artifacts/extensions/snomed_ct_ducklink.wasm` (staged from
//! `~/git/snomed-ct-ducklink-component/target/wasm32-wasip1/release/`)
//! against a SNOMED CT `.duckdb` artifact at
//! `~/snomed-ct/artifacts/current/snomed_ct.duckdb` through `CliHarness`.
//! Proves the WIT `table-macro-registration` record -> duckdb-wasm core
//! `register_pending_table_macro` -> `CREATE OR REPLACE MACRO
//! snomed.subsumed_by(concept_id) AS TABLE (...)` chain against real
//! SNOMED CT relationship data.
//!
//! Skips (rather than fails) when either artifact is missing on disk.

use std::path::{Path, PathBuf};

use ducklink_host::CliHarness;

fn snomed_duckdb_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join("snomed-ct/artifacts/current/snomed_ct.duckdb");
    path.exists().then_some(path)
}

fn snomed_extension_wasm_path() -> Option<PathBuf> {
    let path = ducklink_host::workspace_root().join("artifacts/extensions/snomed_ct_ducklink.wasm");
    path.exists().then_some(path)
}

#[test]
fn snomed_subsumed_by_table_macro_end_to_end() -> anyhow::Result<()> {
    let Some(db_host_path) = snomed_duckdb_path() else {
        eprintln!(
            "skipping: no snomed_ct.duckdb at ~/snomed-ct/artifacts/current/snomed_ct.duckdb"
        );
        return Ok(());
    };
    if snomed_extension_wasm_path().is_none() {
        eprintln!(
            "skipping: no staged snomed_ct_ducklink.wasm at \
             artifacts/extensions/snomed_ct_ducklink.wasm"
        );
        return Ok(());
    }

    let db_dir = db_host_path
        .parent()
        .expect("snomed_ct.duckdb has a parent directory");
    let preopens = [(db_dir, ".")];

    // Self-select any real child SCTID (which by definition has at least one
    // supertype edge) and assert the recursive walk returned real rows with
    // populated `sctid` — a bare COUNT(*) would pass even on the potential
    // `concept_id` parameter-shadow case that d0af965 defended against.
    let sql = "SELECT CASE \
            WHEN COUNT(*) > 0 AND MIN(sctid) IS NOT NULL \
            THEN 'SUBSUMED_OK' ELSE 'SUBSUMED_MISSING' END AS status \
        FROM snomed.subsumed_by((SELECT child FROM is_a_edges LIMIT 1));";

    let args = [
        "duckdb-cli",
        "snomed_ct.duckdb",
        "--load-extension",
        "snomed_ct_ducklink",
        "-c",
        sql,
    ];

    let mut harness = CliHarness::new(&args, &preopens)?;
    let status = harness.run()?;
    let stdout = harness.stdout().unwrap_or_default();
    let stderr = harness.stderr().unwrap_or_default();

    if status.is_err() {
        panic!("snomed.subsumed_by query failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }

    println!("=== snomed.subsumed_by stdout ===\n{stdout}");
    if !stderr.trim().is_empty() {
        println!("=== stderr ===\n{stderr}");
    }

    assert!(
        stdout.contains("SUBSUMED_OK"),
        "expected `SUBSUMED_OK` sentinel in stdout, got:\n{stdout}"
    );
    Ok(())
}
