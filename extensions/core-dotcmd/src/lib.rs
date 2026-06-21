//! A pluggable dot-command component that runs SQL on the CLI's LIVE connection
//! via the `spi` import — so it sees tables the user created this session:
//!   .count TABLE     -> row count
//!   .columns TABLE   -> column names (one per line)
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest};
use duckdb::dotcmd::spi;
struct Component;
const FID_COUNT: u64 = 1;
const FID_COLUMNS: u64 = 2;
/// Double-quote a SQL identifier, escaping embedded quotes.
fn quote_ident(name: &str) -> std::string::String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
/// Single-quote a string literal.
fn quote_str(s: &str) -> std::string::String {
    s.replace('\'', "''")
}
impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        vec![
            CommandSpec { id: FID_COUNT, name: "count".into(), summary: "Row count of a table".into(), usage: "count TABLE".into() },
            CommandSpec { id: FID_COLUMNS, name: "columns".into(), summary: "List a table's column names".into(), usage: "columns TABLE".into() },
        ]
    }
    fn invoke(id: u64, args: String) -> Result<String, String> {
        let table = args.trim();
        if table.is_empty() {
            return Err(format!("usage: .{} TABLE", if id == FID_COUNT { "count" } else { "columns" }));
        }
        match id {
            FID_COUNT => {
                let n = spi::query(&format!("SELECT count(*) FROM {}", quote_ident(table)))?;
                Ok(format!("{} row(s) in {}\n", n.trim(), table))
            }
            FID_COLUMNS => {
                let sql = format!(
                    "SELECT column_name FROM information_schema.columns \
                     WHERE table_name = '{}' ORDER BY ordinal_position",
                    quote_str(table)
                );
                let cols = spi::query(&sql)?;
                if cols.trim().is_empty() {
                    Err(format!("no such table: {table}"))
                } else {
                    Ok(cols)
                }
            }
            other => Err(format!("core-dotcmd: unknown command id {other}")),
        }
    }
}
export!(Component);
