//! End-to-end smoke test for the rxnorm-ducklink-component `rxnorm.tty_class`
//! table macro.
//!
//! Loads `artifacts/extensions/rxnorm_ducklink.wasm` (staged from
//! `~/git/rxnorm-ducklink-component/target/wasm32-wasip1/release/`) against an
//! RxNorm `.duckdb` artifact at `~/rxnorm/artifacts/current/rxnorm.duckdb`
//! through `CliHarness`. Proves the WIT `table-macro-registration` record ->
//! duckdb-wasm core `register_pending_table_macro` -> `CREATE OR REPLACE
//! MACRO rxnorm.tty_class(query_rxcui) AS TABLE (...)` chain against real
//! RxNorm data.
//!
//! Skips (rather than fails) when either artifact is missing on disk.

use std::path::{Path, PathBuf};

use ducklink_host::CliHarness;

fn rxnorm_duckdb_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join("rxnorm/artifacts/current/rxnorm.duckdb");
    path.exists().then_some(path)
}

fn rxnorm_extension_wasm_path() -> Option<PathBuf> {
    let path = ducklink_host::workspace_root().join("artifacts/extensions/rxnorm_ducklink.wasm");
    path.exists().then_some(path)
}

#[test]
fn rxnorm_tty_class_table_macro_end_to_end() -> anyhow::Result<()> {
    let Some(db_host_path) = rxnorm_duckdb_path() else {
        eprintln!("skipping: no rxnorm.duckdb at ~/rxnorm/artifacts/current/rxnorm.duckdb");
        return Ok(());
    };
    if rxnorm_extension_wasm_path().is_none() {
        eprintln!(
            "skipping: no staged rxnorm_ducklink.wasm at artifacts/extensions/rxnorm_ducklink.wasm"
        );
        return Ok(());
    }

    let db_dir = db_host_path
        .parent()
        .expect("rxnorm.duckdb has a parent directory");
    let preopens = [(db_dir, ".")];

    // Self-select any real rxcui and assert the macro returned the single
    // concept row the `concepts` view guarantees with populated `tty` and
    // `name` — a bare COUNT(*) would pass on the shadow-bug case where the
    // WHERE filter got rewritten and the macro returned zero rows.
    let sql = "SELECT CASE \
            WHEN COUNT(*) > 0 AND MIN(tty) IS NOT NULL AND MIN(name) IS NOT NULL \
            THEN 'TTY_OK' ELSE 'TTY_MISSING' END AS status \
        FROM rxnorm.tty_class((SELECT rxcui FROM concepts LIMIT 1));";

    let args = [
        "duckdb-cli",
        "rxnorm.duckdb",
        "--load-extension",
        "rxnorm_ducklink",
        "-c",
        sql,
    ];

    let mut harness = CliHarness::new(&args, &preopens)?;
    let status = harness.run()?;
    let stdout = harness.stdout().unwrap_or_default();
    let stderr = harness.stderr().unwrap_or_default();

    if status.is_err() {
        panic!("rxnorm.tty_class query failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }

    println!("=== rxnorm.tty_class stdout ===\n{stdout}");
    if !stderr.trim().is_empty() {
        println!("=== stderr ===\n{stderr}");
    }

    assert!(
        stdout.contains("TTY_OK"),
        "expected `TTY_OK` sentinel in stdout, got:\n{stdout}"
    );
    Ok(())
}
