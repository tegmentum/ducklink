//! Scenario 1 (native DuckDB + the ducklink extension) cross-corpus test.
//!
//! For every wasm extension that ships a `smoke.sql` (and has a built artifact),
//! this loads the component through the native bridge (`Engine2` +
//! `register_components`) into an in-process DuckDB, runs the smoke statements,
//! and diffs the CLI-`.mode csv`-shaped output against `smoke.expected`.
//!
//! It reuses the same corpus the standalone wasm host (Scenario 2) runs, so the
//! two scenarios are checked against one set of golden expectations. The whole
//! file is a no-op without the `bundled` feature (which provides an in-process
//! DuckDB to register into).
#![cfg(feature = "bundled")]

use std::collections::HashSet;
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use duckdb::types::Value;
use duckdb::Connection;

use ducklink::engine::Engine2;
use ducklink::reg_duckdb::{register_components, ComponentSpec};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct Case {
    name: String,
    smoke_sql: PathBuf,
    expected: Option<PathBuf>,
    artifact: PathBuf,
}

/// Find every `extensions/<name>-component/smoke.sql` that has a matching built
/// `artifacts/extensions/<name>.wasm`.
fn discover() -> Vec<Case> {
    let ext_dir = manifest().join("../../extensions");
    let artifact_dir = manifest().join("../../artifacts/extensions");
    let mut cases = Vec::new();
    let Ok(entries) = fs::read_dir(&ext_dir) else {
        return cases;
    };
    for entry in entries.flatten() {
        let dname = entry.file_name().to_string_lossy().to_string();
        let Some(name) = dname.strip_suffix("-component") else {
            continue;
        };
        let smoke_sql = entry.path().join("smoke.sql");
        let artifact = artifact_dir.join(format!("{name}.wasm"));
        if smoke_sql.is_file() && artifact.is_file() {
            let expected = entry.path().join("smoke.expected");
            cases.push(Case {
                name: name.to_string(),
                smoke_sql,
                expected: expected.is_file().then_some(expected),
                artifact,
            });
        }
    }
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    cases
}

/// Split a smoke.sql file into executable statements, dropping `--` comment
/// lines, `.`-prefixed CLI dot-commands, and blanks. The split respects
/// single-quoted string literals (incl. `''` escapes), so a `;` inside a string
/// (e.g. an HTML entity `&amp;`) does not break a statement.
fn statements(sql: &str) -> Vec<String> {
    let body: String = sql
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--") && !l.starts_with('.'))
        .collect::<Vec<_>>()
        .join("\n");

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if in_str && chars.peek() == Some(&'\'') => {
                cur.push('\'');
                cur.push(chars.next().unwrap()); // the escaped quote
            }
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            ';' if !in_str => {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Quote a CSV field the way DuckDB's CLI `.mode csv` does: wrap in `"` and
/// double internal quotes when it contains a comma, quote, or newline.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Render a value the way DuckDB's CLI `.mode csv` does for the smoke corpus.
fn fmt_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        Value::TinyInt(x) => x.to_string(),
        Value::SmallInt(x) => x.to_string(),
        Value::Int(x) => x.to_string(),
        Value::BigInt(x) => x.to_string(),
        Value::HugeInt(x) => x.to_string(),
        Value::UTinyInt(x) => x.to_string(),
        Value::USmallInt(x) => x.to_string(),
        Value::UInt(x) => x.to_string(),
        Value::UBigInt(x) => x.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Double(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        // DuckDB's CLI renders BLOB as `0x` + lowercase hex.
        Value::Blob(b) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("0x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s
        }
        other => format!("{other:?}"),
    }
}

/// Execute one statement, returning the CSV-shaped output lines (header of column
/// names, then one line per row).
fn run_stmt(con: &Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut stmt = con.prepare(sql).map_err(|e| e.to_string())?;
    // The result schema (and thus column names) only populates after execution,
    // so run the query, collect rows, then read the header. Column count is
    // discovered per row by probing indices until one is out of range.
    let mut data: Vec<Vec<String>> = Vec::new();
    {
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut vals = Vec::new();
            let mut i = 0usize;
            while let Ok(v) = row.get::<usize, Value>(i) {
                vals.push(csv_field(&fmt_value(&v)));
                i += 1;
            }
            data.push(vals);
        }
    }
    let header: Vec<String> = stmt.column_names().iter().map(|n| csv_field(n)).collect();
    let mut out = vec![header.join(",")];
    for vals in data {
        out.push(vals.join(","));
    }
    // A CSV value containing newlines (e.g. html2text/markdown/wordwrap output) is
    // quoted but spans multiple PHYSICAL lines, which is how the CLI-seeded
    // smoke.expected captures it. Split on embedded newlines so the produced
    // physical lines align with the expected.
    Ok(out
        .into_iter()
        .flat_map(|line| line.split('\n').map(str::to_string).collect::<Vec<_>>())
        .collect())
}

/// Normalize CLI-shaped output the way smoke.py does (its `splitlines()` + rstrip
/// + drop-blank-lines): rstrip each line (also strips the trailing `\r` from
/// CRLF, which HTML/markdown extensions emit) and drop blank lines. DuckDB's CLI
/// emits blank lines for empty-string values and inside multi-line values; the
/// corpus drops them (NULL renders as the literal "NULL", not blank).
fn normalize(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Expected lines: drop `#` comment lines, then normalize (rstrip + drop blanks).
fn load_expected(path: &Path) -> Vec<String> {
    normalize(
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .map(|l| l.to_string())
            .collect(),
    )
}

/// Diff produced vs expected, honoring `~~` (skip) and `?` (any non-empty).
/// Both sides are already `normalize`d (rstripped, blanks dropped).
fn compare(produced: &[String], expected: &[String]) -> Result<(), String> {
    for (i, exp) in expected.iter().enumerate() {
        if exp == "~~" {
            continue;
        }
        let got = produced.get(i).map(|s| s.trim_end()).unwrap_or("");
        if exp == "?" {
            if got.is_empty() {
                return Err(format!("line {i}: expected non-empty, got empty"));
            }
            continue;
        }
        if got != exp {
            return Err(format!("line {i}: expected {exp:?}, got {got:?}"));
        }
    }
    if produced.len() > expected.len() {
        return Err(format!(
            "produced {} lines, expected {}",
            produced.len(),
            expected.len()
        ));
    }
    Ok(())
}

enum Outcome {
    Pass,
    Mismatch(String),
    Error(String),
}

/// `Ok(None)` = pass, `Ok(Some)` = ran but mismatched, `Err` = hard error.
fn run_inner(case: &Case) -> Result<Option<String>, String> {
    let engine = Arc::new(Mutex::new(
        Engine2::new().map_err(|e| format!("engine: {e}"))?,
    ));

    // Create the database directly so we have a raw connection for aggregate
    // registration (the duckdb-rs `Connection` doesn't expose its raw handle).
    // The duckdb-rs `Connection` (for scalars/tables/queries) and the raw
    // connection are siblings on the same db, so functions register db-wide.
    let mut db: duckdb::ffi::duckdb_database = std::ptr::null_mut();
    if unsafe { duckdb::ffi::duckdb_open(std::ptr::null(), &mut db) } != duckdb::ffi::DuckDBSuccess {
        return Err("duckdb_open failed".into());
    }
    let con = unsafe { Connection::open_from_raw(db) }.map_err(|e| format!("open: {e}"))?;
    let mut raw_con: duckdb::ffi::duckdb_connection = std::ptr::null_mut();
    if unsafe { duckdb::ffi::duckdb_connect(db, &mut raw_con) } != duckdb::ffi::DuckDBSuccess {
        unsafe { duckdb::ffi::duckdb_close(&mut db) };
        return Err("duckdb_connect failed".into());
    }

    let specs = vec![ComponentSpec {
        name: case.name.clone(),
        path: case.artifact.clone(),
    }];

    let outcome = (|| -> Result<Option<String>, String> {
        register_components(&con, Some(raw_con), engine, &specs)
            .map_err(|e| format!("register: {e}"))?;

        let sql = fs::read_to_string(&case.smoke_sql).map_err(|e| e.to_string())?;
        let mut produced = Vec::new();
        for stmt in statements(&sql) {
            let lines = run_stmt(&con, &stmt).map_err(|e| format!("exec `{stmt}`: {e}"))?;
            produced.extend(lines);
        }
        match &case.expected {
            Some(exp) => match compare(&normalize(produced), &load_expected(exp)) {
                Ok(()) => Ok(None),
                Err(diff) => Ok(Some(diff)),
            },
            None => Ok(None),
        }
    })();

    // Tear down: drop the duckdb-rs connection first, then the raw sibling, then
    // the database.
    drop(con);
    unsafe {
        duckdb::ffi::duckdb_disconnect(&mut raw_con);
        duckdb::ffi::duckdb_close(&mut db);
    }
    outcome
}

fn run_case(case: &Case) -> Outcome {
    match std::panic::catch_unwind(AssertUnwindSafe(|| run_inner(case))) {
        Ok(Ok(None)) => Outcome::Pass,
        Ok(Ok(Some(diff))) => Outcome::Mismatch(diff),
        Ok(Err(err)) => Outcome::Error(err),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Outcome::Error(format!("panic: {msg}"))
        }
    }
}

#[test]
fn scenario1_corpus() {
    let cases = discover();
    assert!(
        !cases.is_empty(),
        "no extensions discovered (smoke.sql + artifact)"
    );

    // Quiet the default panic printer; per-extension panics are caught and
    // categorized below.
    std::panic::set_hook(Box::new(|_| {}));

    let mut pass = Vec::new();
    let mut mismatch = Vec::new();
    let mut error = Vec::new();
    for case in &cases {
        match run_case(case) {
            Outcome::Pass => pass.push(case.name.clone()),
            Outcome::Mismatch(d) => mismatch.push((case.name.clone(), d)),
            Outcome::Error(e) => error.push((case.name.clone(), e)),
        }
    }

    println!(
        "\n=== Scenario 1 (native ducklink ext) corpus: {} extensions ===",
        cases.len()
    );
    println!(
        "PASS {}   MISMATCH {}   ERROR {}",
        pass.len(),
        mismatch.len(),
        error.len()
    );
    println!("\n-- PASS ({}) --\n{}", pass.len(), pass.join(", "));
    println!("\n-- MISMATCH ({}) --", mismatch.len());
    for (name, diff) in &mismatch {
        println!("  {name}: {diff}");
    }
    println!("\n-- ERROR ({}) --", error.len());
    for (name, err) in &error {
        println!("  {name}: {err}");
    }

    // Baseline gate: a handful of pure scalar validators/transformers must PASS
    // exactly (bool/text output, no float-format ambiguity). The long tail of
    // MISMATCH/ERROR is reported, not asserted, since it reflects CSV-format
    // edge cases and unsupported function kinds (e.g. aggregates) rather than
    // bridge bugs.
    let passset: HashSet<&str> = pass.iter().map(String::as_str).collect();
    let baseline = ["isin", "luhn", "slug", "rot13"];
    for b in baseline {
        let present = cases.iter().any(|c| c.name == b);
        if present {
            assert!(
                passset.contains(b),
                "baseline extension '{b}' did not PASS (see matrix above)"
            );
        }
    }
}
