//! sqlite-utils "FTS" commands ported to the DuckDB dot-command world.
//! These wrap DuckDB's `fts` extension (PRAGMA create_fts_index / drop_fts_index
//! and the fts_main_<table>.match_bm25 search function). Everything runs on the
//! CLI's live connection via `spi`. The fts extension must be available; any
//! "extension not loaded" error simply propagates from spi.
//!   .enable_fts TABLE ID_COL COL [COL ...]   build an FTS index
//!   .disable_fts TABLE                        drop the FTS index
//!   .rebuild_fts TABLE ID_COL COL [COL ...]   rebuild (overwrite) the FTS index
//!   .search TABLE ID_COL QUERY...             BM25 full-text search
//! sqlite-utils `populate_fts` is omitted because DuckDB builds the index
//! directly from the source table (no separate populate step).
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};
use duckdb::dotcmd::spi;

struct Component;

const FID_ENABLE: u64 = 1;
const FID_DISABLE: u64 = 2;
const FID_REBUILD: u64 = 3;
const FID_SEARCH: u64 = 4;

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

/// Single-quote a string as a SQL string literal (PRAGMA arg), escaping `'`.
fn lit(s: &str) -> std::string::String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        let c = |id, name: &str, summary: &str, usage: &str| CommandSpec {
            id, name: name.into(), summary: summary.into(), usage: usage.into(),
        };
        vec![
            c(FID_ENABLE, "enable_fts", "Build an FTS index",
              "enable_fts TABLE ID_COL COL [COL ...]"),
            c(FID_DISABLE, "disable_fts", "Drop the FTS index", "disable_fts TABLE"),
            c(FID_REBUILD, "rebuild_fts", "Rebuild (overwrite) the FTS index",
              "rebuild_fts TABLE ID_COL COL [COL ...]"),
            c(FID_SEARCH, "search", "BM25 full-text search", "search TABLE ID_COL QUERY..."),
        ]
    }

    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        let t = tokens(&args);
        match id {
            FID_ENABLE => {
                if t.len() < 3 {
                    return Err("usage: .enable_fts TABLE ID_COL COL [COL ...]".into());
                }
                let table = t[0];
                let ncols = t.len() - 2;
                let pargs: Vec<std::string::String> = t.iter().map(|a| lit(a)).collect();
                run(
                    &format!("PRAGMA create_fts_index({})", pargs.join(", ")),
                    format!("enabled FTS on {table} ({ncols} column(s))"),
                )
            }

            FID_DISABLE => {
                let table = t.first().ok_or("usage: .disable_fts TABLE")?;
                run(
                    &format!("PRAGMA drop_fts_index({})", lit(table)),
                    format!("disabled FTS on {table}"),
                )
            }

            FID_REBUILD => {
                if t.len() < 3 {
                    return Err("usage: .rebuild_fts TABLE ID_COL COL [COL ...]".into());
                }
                let table = t[0];
                let pargs: Vec<std::string::String> = t.iter().map(|a| lit(a)).collect();
                run(
                    &format!("PRAGMA create_fts_index({}, overwrite=1)", pargs.join(", ")),
                    format!("rebuilt FTS on {table}"),
                )
            }

            FID_SEARCH => {
                if t.len() < 3 {
                    return Err("usage: .search TABLE ID_COL QUERY...".into());
                }
                let table = t[0];
                let id_col = t[1];
                // QUERY is the raw text after the first two tokens.
                let after_table = args.trim_start().strip_prefix(table).unwrap_or("");
                let after_id = after_table.trim_start().strip_prefix(id_col).unwrap_or("");
                let query = after_id.trim();
                if query.is_empty() {
                    return Err("usage: .search TABLE ID_COL QUERY...".into());
                }
                let qtable = quote_ident(table);
                let qid_col = quote_ident(id_col);
                let sql = format!(
                    "SELECT * FROM (SELECT *, fts_main_{table}.match_bm25({qid_col}, {q}) \
                     AS score FROM {qtable}) sq WHERE score IS NOT NULL ORDER BY score DESC",
                    table = table, qid_col = qid_col, q = lit(query), qtable = qtable,
                );
                Ok(plain(spi::query(&sql)?))
            }

            other => Err(format!("duckdb-utils-fts: unknown command id {other}")),
        }
    }
}
export!(Component);
