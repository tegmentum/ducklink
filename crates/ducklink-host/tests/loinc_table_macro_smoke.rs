//! End-to-end smoke test for the loinc-ducklink-component `loinc.hierarchy`
//! table macro.
//!
//! Loads `artifacts/extensions/loinc_ducklink.wasm` (staged from
//! `~/git/loinc-ducklink-component/target/wasm32-wasip1/release/`) against a
//! LOINC `.duckdb` artifact at `~/loinc/artifacts/current/loinc.duckdb`
//! through `CliHarness`. Proves the WIT `table-macro-registration` record ->
//! duckdb-wasm core `register_pending_table_macro` -> `CREATE OR REPLACE
//! MACRO loinc.hierarchy(code) AS TABLE (...)` chain against real LOINC data.
//!
//! Skips (rather than fails) when either artifact is missing on disk.

use std::path::{Path, PathBuf};

use ducklink_host::CliHarness;

fn loinc_duckdb_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join("loinc/artifacts/current/loinc.duckdb");
    path.exists().then_some(path)
}

fn loinc_extension_wasm_path() -> Option<PathBuf> {
    let path = ducklink_host::workspace_root().join("artifacts/extensions/loinc_ducklink.wasm");
    path.exists().then_some(path)
}

#[test]
fn loinc_hierarchy_table_macro_end_to_end() -> anyhow::Result<()> {
    let Some(db_host_path) = loinc_duckdb_path() else {
        eprintln!("skipping: no loinc.duckdb at ~/loinc/artifacts/current/loinc.duckdb");
        return Ok(());
    };
    if loinc_extension_wasm_path().is_none() {
        eprintln!(
            "skipping: no staged loinc_ducklink.wasm at artifacts/extensions/loinc_ducklink.wasm"
        );
        return Ok(());
    }

    let db_dir = db_host_path
        .parent()
        .expect("loinc.duckdb has a parent directory");
    let preopens = [(db_dir, ".")];

    // Self-select any child LOINC (which by definition has at least one
    // parent edge) and assert the macro walked the recursive CTE, joined
    // concepts + multi_axial_hierarchy, and returned real rows with
    // populated `loinc_num` — a bare COUNT(*) would pass on the empty case
    // that shipped the parameter-shadow bug (ba4c389).
    let sql = "SELECT CASE \
            WHEN COUNT(*) > 0 AND MIN(loinc_num) IS NOT NULL \
            THEN 'HIERARCHY_OK' ELSE 'HIERARCHY_MISSING' END AS status \
        FROM loinc.hierarchy((SELECT child FROM hierarchy_edges LIMIT 1));";

    let args = [
        "duckdb-cli",
        "loinc.duckdb",
        "--load-extension",
        "loinc_ducklink",
        "-c",
        sql,
    ];

    let mut harness = CliHarness::new(&args, &preopens)?;
    let status = harness.run()?;
    let stdout = harness.stdout().unwrap_or_default();
    let stderr = harness.stderr().unwrap_or_default();

    if status.is_err() {
        panic!("loinc.hierarchy query failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }

    println!("=== loinc.hierarchy stdout ===\n{stdout}");
    if !stderr.trim().is_empty() {
        println!("=== stderr ===\n{stderr}");
    }

    assert!(
        stdout.contains("HIERARCHY_OK"),
        "expected `HIERARCHY_OK` sentinel in stdout, got:\n{stdout}"
    );
    Ok(())
}
