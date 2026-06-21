//! The built-in dot commands, as a pluggable component. Everything here runs on
//! the CLI's LIVE connection via `spi`, or mutates session state via deltas:
//!   .tables [LIKE]   .schema [TABLE]   .indexes [TABLE]   — introspection (spi)
//!   .count TABLE     .columns TABLE                        — convenience (spi)
//!   .json .csv .box                                        — output mode (delta)
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult, StateDelta};
use duckdb::dotcmd::spi;
struct Component;
const FID_COUNT: u64 = 1;
const FID_COLUMNS: u64 = 2;
const FID_JSON: u64 = 3;
const FID_CSV: u64 = 4;
const FID_BOX: u64 = 5;
const FID_TABLES: u64 = 6;
const FID_SCHEMA: u64 = 7;
const FID_INDEXES: u64 = 8;
const USER_SCHEMA: &str = "table_schema NOT IN ('information_schema', 'pg_catalog')";
fn quote_ident(name: &str) -> std::string::String { format!("\"{}\"", name.replace('"', "\"\"")) }
fn quote_str(s: &str) -> std::string::String { s.replace('\'', "''") }
fn plain(text: std::string::String) -> InvokeResult { InvokeResult { text, state_deltas: vec![] } }
fn set_mode(mode: &str, note: &str) -> InvokeResult {
    InvokeResult {
        text: format!("{note}\n"),
        state_deltas: vec![StateDelta { key: "display/mode".into(), value: mode.into() }],
    }
}
impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        let c = |id, name: &str, summary: &str, usage: &str| CommandSpec {
            id, name: name.into(), summary: summary.into(), usage: usage.into(),
        };
        vec![
            c(FID_TABLES, "tables", "List tables (optional LIKE pattern)", "tables [LIKE]"),
            c(FID_SCHEMA, "schema", "Show CREATE statements", "schema [TABLE]"),
            c(FID_INDEXES, "indexes", "List indexes", "indexes [TABLE]"),
            c(FID_COUNT, "count", "Row count of a table", "count TABLE"),
            c(FID_COLUMNS, "columns", "List a table's column names", "columns TABLE"),
            c(FID_JSON, "json", "Switch output to JSON", "json"),
            c(FID_CSV, "csv", "Switch output to CSV", "csv"),
            c(FID_BOX, "box", "Switch output to the box table", "box"),
        ]
    }
    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        let arg = args.trim();
        match id {
            FID_JSON => return Ok(set_mode("json", "output mode -> json")),
            FID_CSV => return Ok(set_mode("csv", "output mode -> csv")),
            FID_BOX => return Ok(set_mode("table", "output mode -> box")),
            FID_TABLES => {
                let filter = if arg.is_empty() { std::string::String::new() }
                    else { format!(" AND table_name LIKE '{}'", quote_str(arg)) };
                return Ok(plain(spi::query(&format!(
                    "SELECT table_name FROM information_schema.tables \
                     WHERE {USER_SCHEMA}{filter} ORDER BY table_name"
                ))?));
            }
            FID_SCHEMA => {
                let filter = if arg.is_empty() { std::string::String::new() }
                    else { format!(" WHERE table_name = '{}'", quote_str(arg)) };
                let out = spi::query(&format!(
                    "SELECT sql FROM duckdb_tables(){filter} ORDER BY table_name"
                ))?;
                return Ok(plain(if out.trim().is_empty() && !arg.is_empty() {
                    format!("no such table: {arg}\n")
                } else { out }));
            }
            FID_INDEXES => {
                let filter = if arg.is_empty() { std::string::String::new() }
                    else { format!(" WHERE table_name = '{}'", quote_str(arg)) };
                return Ok(plain(spi::query(&format!(
                    "SELECT index_name FROM duckdb_indexes(){filter} ORDER BY index_name"
                ))?));
            }
            _ => {}
        }
        // count / columns require a table argument.
        if arg.is_empty() {
            return Err(format!("usage: .{} TABLE", if id == FID_COUNT { "count" } else { "columns" }));
        }
        match id {
            FID_COUNT => {
                let n = spi::query(&format!("SELECT count(*) FROM {}", quote_ident(arg)))?;
                Ok(plain(format!("{} row(s) in {}\n", n.trim(), arg)))
            }
            FID_COLUMNS => {
                let cols = spi::query(&format!(
                    "SELECT column_name FROM information_schema.columns \
                     WHERE table_name = '{}' ORDER BY ordinal_position",
                    quote_str(arg)
                ))?;
                if cols.trim().is_empty() { Err(format!("no such table: {arg}")) } else { Ok(plain(cols)) }
            }
            other => Err(format!("core-dotcmd: unknown command id {other}")),
        }
    }
}
export!(Component);
