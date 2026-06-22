//! sqlite-utils "maintenance" commands ported to the DuckDB dot-command world.
//! Everything runs on the CLI's live connection via `spi`.
//!   .vacuum        reclaim space (VACUUM)
//!   .analyze       refresh whole-database statistics (ANALYZE)
//!   .checkpoint    flush the WAL to the database file (CHECKPOINT)
//!   .db_size       database size / block stats (CALL pragma_database_size())
//! OMITTED (no DuckDB equivalent): `optimize` (no PRAGMA optimize),
//! `enable_wal`/`disable_wal` (DuckDB manages its own WAL; no user pragma), and
//! `enable_counts`/`reset_counts` (DuckDB has no triggers).
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};
use duckdb::dotcmd::spi;

struct Component;

const FID_VACUUM: u64 = 1;
const FID_ANALYZE: u64 = 2;
const FID_CHECKPOINT: u64 = 3;
const FID_DB_SIZE: u64 = 4;

#[allow(dead_code)]
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
#[allow(dead_code)]
fn tokens(args: &str) -> Vec<&str> {
    args.split_whitespace().collect()
}

/// Run DDL/DML and, on success, return `ok_msg` rather than the (usually empty)
/// result text.
fn run(sql: &str, ok_msg: std::string::String) -> Result<InvokeResult, String> {
    spi::query(sql)?;
    Ok(note(ok_msg))
}

impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        let c = |id, name: &str, summary: &str, usage: &str| CommandSpec {
            id, name: name.into(), summary: summary.into(), usage: usage.into(),
        };
        vec![
            c(FID_VACUUM, "vacuum", "Reclaim space (VACUUM)", "vacuum"),
            c(FID_ANALYZE, "analyze", "Refresh whole-database statistics (ANALYZE)", "analyze"),
            c(FID_CHECKPOINT, "checkpoint", "Flush the WAL to the database file (CHECKPOINT)", "checkpoint"),
            c(FID_DB_SIZE, "db_size", "Database size / block stats", "db_size"),
        ]
    }

    fn invoke(id: u64, _args: String) -> Result<InvokeResult, String> {
        match id {
            // DuckDB has no per-table ANALYZE; any table token is ignored.
            FID_VACUUM => run("VACUUM", "vacuum complete".into()),
            FID_ANALYZE => run("ANALYZE", "analyze complete".into()),
            FID_CHECKPOINT => run("CHECKPOINT", "checkpoint complete".into()),
            FID_DB_SIZE => Ok(plain(spi::query("CALL pragma_database_size()")?)),
            other => Err(format!("duckdb-utils-maint: unknown command id {other}")),
        }
    }
}
export!(Component);
