//! Pluggable dot commands that touch the live session:
//!   .count TABLE / .columns TABLE   — query the live connection (spi)
//!   .json / .csv / .box             — change the CLI's output mode via a
//!                                     `display/mode` state-delta
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
fn quote_ident(name: &str) -> std::string::String { format!("\"{}\"", name.replace('"', "\"\"")) }
fn quote_str(s: &str) -> std::string::String { s.replace('\'', "''") }
fn plain(text: std::string::String) -> InvokeResult { InvokeResult { text, state_deltas: vec![] } }
/// A result that prints `note` and tells the CLI to switch output mode.
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
            c(FID_COUNT, "count", "Row count of a table", "count TABLE"),
            c(FID_COLUMNS, "columns", "List a table's column names", "columns TABLE"),
            c(FID_JSON, "json", "Switch output to JSON", "json"),
            c(FID_CSV, "csv", "Switch output to CSV", "csv"),
            c(FID_BOX, "box", "Switch output to the box table", "box"),
        ]
    }
    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        match id {
            FID_JSON => return Ok(set_mode("json", "output mode -> json")),
            FID_CSV => return Ok(set_mode("csv", "output mode -> csv")),
            FID_BOX => return Ok(set_mode("table", "output mode -> box")),
            _ => {}
        }
        let table = args.trim();
        if table.is_empty() {
            return Err(format!("usage: .{} TABLE", if id == FID_COUNT { "count" } else { "columns" }));
        }
        match id {
            FID_COUNT => {
                let n = spi::query(&format!("SELECT count(*) FROM {}", quote_ident(table)))?;
                Ok(plain(format!("{} row(s) in {}\n", n.trim(), table)))
            }
            FID_COLUMNS => {
                let sql = format!(
                    "SELECT column_name FROM information_schema.columns \
                     WHERE table_name = '{}' ORDER BY ordinal_position",
                    quote_str(table)
                );
                let cols = spi::query(&sql)?;
                if cols.trim().is_empty() { Err(format!("no such table: {table}")) } else { Ok(plain(cols)) }
            }
            other => Err(format!("core-dotcmd: unknown command id {other}")),
        }
    }
}
export!(Component);
