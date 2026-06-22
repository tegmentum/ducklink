//! sqlite-utils "data" commands ported to the DuckDB dot-command world.
//! Everything runs on the CLI's live connection via `spi`.
//!   .rows TABLE [LIMIT]             show rows from a table
//!   .analyze_tables [TABLE]        per-column stats for a table (or all tables)
//!   .insert TABLE FILE             load a json/csv/tsv/parquet file into a table
//!   .upsert TABLE FILE --pk COL    upsert a file into a table on the given pk
//!   .convert TABLE COL EXPR ...    UPDATE TABLE SET COL = EXPR
//!   .insert_files TABLE FILE ...   store files (path, content, size) in a table
//!   .memory FILE [NAME]            load a file into a temp table you can query
//!   .bulk FILE SQL ...             run SQL against the file exposed as `data`
//! DuckDB adaptation: sqlite-utils `memory` reads stdin and `bulk` binds named
//! params per row; spi exposes neither, so both take a FILE — `memory`
//! materializes it as a temp table, and `bulk` exposes it as a relation named
//! `data` that the provided (set-based) SQL references.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};
use duckdb::dotcmd::spi;

struct Component;

const FID_ROWS: u64 = 1;
const FID_ANALYZE_TABLES: u64 = 2;
const FID_INSERT: u64 = 3;
const FID_UPSERT: u64 = 4;
const FID_CONVERT: u64 = 5;
const FID_INSERT_FILES: u64 = 6;
const FID_MEMORY: u64 = 7;
const FID_BULK: u64 = 8;

/// Derive a SQL identifier from a file path (stem, non-alnum -> '_').
fn name_from_file(file: &str) -> std::string::String {
    let stem = file.rsplit('/').next().unwrap_or(file);
    let stem = stem.split('.').next().unwrap_or(stem);
    let cleaned: std::string::String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if cleaned.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) {
        format!("t_{cleaned}")
    } else {
        cleaned
    }
}

fn quote_ident(name: &str) -> std::string::String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn plain(text: std::string::String) -> InvokeResult {
    InvokeResult { text, state_deltas: vec![] }
}
fn note(text: std::string::String) -> InvokeResult {
    plain(if text.ends_with('\n') { text } else { format!("{text}\n") })
}

/// Split a raw arg string into whitespace-separated tokens.
fn tokens(args: &str) -> Vec<&str> {
    args.split_whitespace().collect()
}

/// Run DDL/DML and, on success, return `ok_msg` rather than the (usually empty)
/// result text.
fn run(sql: &str, ok_msg: std::string::String) -> Result<InvokeResult, String> {
    spi::query(sql)?;
    Ok(note(ok_msg))
}

/// Single-quote-escape a string for inlining into SQL.
fn sql_str(s: &str) -> std::string::String {
    s.replace('\'', "''")
}

/// Pick a DuckDB table function from a file extension.
fn reader_for(file: &str) -> Result<&'static str, String> {
    let lower = file.to_lowercase();
    if lower.ends_with(".json") || lower.ends_with(".jsonl") || lower.ends_with(".ndjson") {
        Ok("read_json_auto")
    } else if lower.ends_with(".csv") || lower.ends_with(".tsv") {
        Ok("read_csv_auto")
    } else if lower.ends_with(".parquet") {
        Ok("read_parquet")
    } else {
        Err("unsupported file type".into())
    }
}

/// Get a table's column names in ordinal order.
fn columns_of(table: &str) -> Result<Vec<std::string::String>, String> {
    let rows = spi::query(&format!(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = '{}' ORDER BY ordinal_position",
        sql_str(table)
    ))?;
    Ok(rows
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        let c = |id, name: &str, summary: &str, usage: &str| CommandSpec {
            id, name: name.into(), summary: summary.into(), usage: usage.into(),
        };
        vec![
            c(FID_ROWS, "rows", "Show rows from a table", "rows TABLE [LIMIT]"),
            c(FID_ANALYZE_TABLES, "analyze_tables", "Per-column stats for a table (or all tables)",
              "analyze_tables [TABLE]"),
            c(FID_INSERT, "insert", "Load a json/csv/tsv/parquet file into a table",
              "insert TABLE FILE"),
            c(FID_UPSERT, "upsert", "Upsert a file into a table on the given pk",
              "upsert TABLE FILE --pk COL[,COL]"),
            c(FID_CONVERT, "convert", "UPDATE TABLE SET COL = EXPR", "convert TABLE COL EXPR ..."),
            c(FID_INSERT_FILES, "insert_files", "Store files (path, content, size) in a table",
              "insert_files TABLE FILE [FILE ...]"),
            c(FID_MEMORY, "memory", "Load a file into a queryable temp table",
              "memory FILE [NAME]"),
            c(FID_BULK, "bulk", "Run SQL against a file exposed as relation `data`",
              "bulk FILE SQL ..."),
        ]
    }

    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        let t = tokens(&args);
        match id {
            FID_ROWS => {
                let table = t.first().ok_or("usage: .rows TABLE [LIMIT]")?;
                let limit = t.get(1).and_then(|s| s.parse::<i64>().ok());
                let sql = match limit {
                    Some(n) => format!("SELECT * FROM {} LIMIT {}", quote_ident(table), n),
                    None => format!("SELECT * FROM {}", quote_ident(table)),
                };
                Ok(plain(spi::query(&sql)?))
            }

            FID_ANALYZE_TABLES => {
                // Resolve the table list.
                let tables: Vec<std::string::String> = match t.first() {
                    Some(tbl) => vec![tbl.to_string()],
                    None => {
                        let rows = spi::query(
                            "SELECT table_name FROM information_schema.tables \
                             WHERE table_schema NOT IN ('information_schema','pg_catalog') \
                             ORDER BY table_name",
                        )?;
                        rows.lines()
                            .map(|l| l.trim())
                            .filter(|l| !l.is_empty())
                            .map(|l| l.to_string())
                            .collect()
                    }
                };
                let mut out: Vec<std::string::String> = vec![];
                for table in &tables {
                    let cols = columns_of(table)?;
                    if cols.is_empty() {
                        continue;
                    }
                    let qtable = quote_ident(table);
                    let tbl_lit = sql_str(table);
                    let selects: Vec<std::string::String> = cols
                        .iter()
                        .map(|c| {
                            let qc = quote_ident(c);
                            format!(
                                "SELECT '{}' AS tbl, '{}' AS column, \
                                 COUNT(DISTINCT {qc}) AS n_distinct, \
                                 COUNT(*) - COUNT({qc}) AS nulls, \
                                 MIN({qc})::VARCHAR AS min, MAX({qc})::VARCHAR AS max \
                                 FROM {qtable}",
                                tbl_lit, sql_str(c)
                            )
                        })
                        .collect();
                    out.push(spi::query(&selects.join(" UNION ALL "))?);
                }
                Ok(plain(out.join("\n")))
            }

            FID_INSERT => {
                if t.len() < 2 {
                    return Err("usage: .insert TABLE FILE".into());
                }
                let table = t[0];
                let file = t[1];
                let reader = reader_for(file)?;
                let qtable = quote_ident(table);
                let f = sql_str(file);
                spi::query(&format!(
                    "CREATE TABLE IF NOT EXISTS {qtable} AS \
                     SELECT * FROM {reader}('{f}') WHERE false"
                ))?;
                spi::query(&format!(
                    "INSERT INTO {qtable} SELECT * FROM {reader}('{f}')"
                ))?;
                Ok(note(format!("inserted rows from {file} into {table}")))
            }

            FID_UPSERT => {
                if t.len() < 2 {
                    return Err("usage: .upsert TABLE FILE --pk COL[,COL]".into());
                }
                let table = t[0];
                let file = t[1];
                let reader = reader_for(file)?;
                let pk_arg = {
                    let mut found: Option<&str> = None;
                    let mut i = 2;
                    while i < t.len() {
                        if t[i] == "--pk" {
                            found = t.get(i + 1).copied();
                            break;
                        }
                        i += 1;
                    }
                    found.ok_or("usage: .upsert TABLE FILE --pk COL[,COL]")?
                };
                let pks: Vec<&str> =
                    pk_arg.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                if pks.is_empty() {
                    return Err("--pk needs at least one column".into());
                }
                let qtable = quote_ident(table);
                let f = sql_str(file);
                let pk_list = pks.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
                let cols = columns_of(table)?;
                let non_pk: Vec<std::string::String> = cols
                    .iter()
                    .filter(|c| !pks.iter().any(|p| p == &c.as_str()))
                    .map(|c| {
                        let qc = quote_ident(c);
                        format!("{qc} = EXCLUDED.{qc}")
                    })
                    .collect();
                let conflict = if non_pk.is_empty() {
                    format!("ON CONFLICT ({pk_list}) DO NOTHING")
                } else {
                    format!("ON CONFLICT ({pk_list}) DO UPDATE SET {}", non_pk.join(", "))
                };
                spi::query(&format!(
                    "INSERT INTO {qtable} SELECT * FROM {reader}('{f}') {conflict}"
                ))?;
                Ok(note(format!("upserted {file} into {table}")))
            }

            FID_CONVERT => {
                if t.len() < 3 {
                    return Err("usage: .convert TABLE COL EXPR ...".into());
                }
                let table = t[0];
                let col = t[1];
                // EXPR is the remainder of the raw args after TABLE and COL,
                // preserving original spacing.
                let rest = args.trim_start();
                let rest = rest.strip_prefix(table).unwrap_or(rest).trim_start();
                let expr = rest.strip_prefix(col).unwrap_or(rest).trim();
                if expr.is_empty() {
                    return Err("usage: .convert TABLE COL EXPR ...".into());
                }
                run(
                    &format!("UPDATE {} SET {} = {}", quote_ident(table), quote_ident(col), expr),
                    format!("converted {table}.{col}"),
                )
            }

            FID_INSERT_FILES => {
                if t.len() < 2 {
                    return Err("usage: .insert_files TABLE FILE [FILE ...]".into());
                }
                let table = t[0];
                let files = &t[1..];
                let qtable = quote_ident(table);
                spi::query(&format!(
                    "CREATE TABLE IF NOT EXISTS {qtable}(path VARCHAR, content BLOB, size BIGINT)"
                ))?;
                let list = files
                    .iter()
                    .map(|f| format!("'{}'", sql_str(f)))
                    .collect::<Vec<_>>()
                    .join(", ");
                spi::query(&format!(
                    "INSERT INTO {qtable}(path, content, size) \
                     SELECT filename, content, size FROM read_blob([{list}])"
                ))?;
                Ok(note(format!("inserted {} file(s) into {table}", files.len())))
            }

            FID_MEMORY => {
                let file = t.first().ok_or("usage: .memory FILE [NAME]")?;
                let reader = reader_for(file)?;
                let name = t.get(1).map(|s| s.to_string()).unwrap_or_else(|| name_from_file(file));
                run(
                    &format!(
                        "CREATE OR REPLACE TEMP TABLE {} AS SELECT * FROM {reader}('{}')",
                        quote_ident(&name), sql_str(file)
                    ),
                    format!("loaded {file} as temp table {name}"),
                )
            }

            FID_BULK => {
                // Expose FILE as a temp relation `data`, run the user's SQL, drop it.
                if t.len() < 2 {
                    return Err("usage: .bulk FILE SQL ...".into());
                }
                let file = t[0];
                let reader = reader_for(file)?;
                let sql = args.trim_start();
                let sql = sql.strip_prefix(file).unwrap_or(sql).trim();
                if sql.is_empty() {
                    return Err("usage: .bulk FILE SQL ...".into());
                }
                spi::query(&format!(
                    "CREATE OR REPLACE TEMP VIEW data AS SELECT * FROM {reader}('{}')",
                    sql_str(file)
                ))?;
                let result = spi::query(sql);
                // Always drop the helper view, regardless of the statement outcome.
                let _ = spi::query("DROP VIEW IF EXISTS data");
                let out = result?;
                Ok(if out.trim().is_empty() {
                    note(format!("bulk applied SQL over {file}"))
                } else {
                    plain(out)
                })
            }

            other => Err(format!("duckdb-utils-data: unknown command id {other}")),
        }
    }
}
export!(Component);
