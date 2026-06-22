//! sqlite-utils "schema" commands ported to the DuckDB dot-command world.
//! Everything runs on the CLI's live connection via `spi`. Commands that have no
//! DuckDB equivalent (triggers, post-hoc ADD FOREIGN KEY) are intentionally
//! omitted; DuckDB has no triggers and ALTER TABLE can't add FK constraints.
//!   .views                          list views
//!   .create_table NAME C:T ...      create a table from a name:type colspec
//!   .create_index TABLE C [C ...]   create an index
//!   .create_view NAME SELECT ...    create a view
//!   .drop_table NAME [--ignore]     drop a table
//!   .drop_view NAME [--ignore]      drop a view
//!   .rename_table OLD NEW           rename a table
//!   .duplicate OLD NEW              copy a table (schema + rows)
//!   .add_column TABLE COL TYPE      add a column
//!   .transform TABLE --rename a:b --drop c --type c:T   alter columns
//!   .index_fks [TABLE]             create an index on every foreign-key column
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};
use duckdb::dotcmd::spi;

struct Component;

const FID_VIEWS: u64 = 1;
const FID_CREATE_TABLE: u64 = 2;
const FID_CREATE_INDEX: u64 = 3;
const FID_CREATE_VIEW: u64 = 4;
const FID_DROP_TABLE: u64 = 5;
const FID_DROP_VIEW: u64 = 6;
const FID_RENAME_TABLE: u64 = 7;
const FID_DUPLICATE: u64 = 8;
const FID_ADD_COLUMN: u64 = 9;
const FID_TRANSFORM: u64 = 10;
const FID_INDEX_FKS: u64 = 11;

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

/// Map a sqlite-utils-style type alias to a DuckDB type. Unknown -> uppercased.
fn map_type(t: &str) -> std::string::String {
    match t.to_lowercase().as_str() {
        "int" | "integer" => "INTEGER".into(),
        "float" | "real" | "double" => "DOUBLE".into(),
        "text" | "str" | "string" => "VARCHAR".into(),
        "bool" | "boolean" => "BOOLEAN".into(),
        "blob" | "bytes" => "BLOB".into(),
        other => other.to_uppercase(),
    }
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
            c(FID_VIEWS, "views", "List views", "views"),
            c(FID_CREATE_TABLE, "create_table", "Create a table from a name:type colspec",
              "create_table NAME COL:TYPE [COL:TYPE ...] [--pk COL]"),
            c(FID_CREATE_INDEX, "create_index", "Create an index", "create_index TABLE COL [COL ...]"),
            c(FID_CREATE_VIEW, "create_view", "Create a view", "create_view NAME SELECT ..."),
            c(FID_DROP_TABLE, "drop_table", "Drop a table", "drop_table NAME [--ignore]"),
            c(FID_DROP_VIEW, "drop_view", "Drop a view", "drop_view NAME [--ignore]"),
            c(FID_RENAME_TABLE, "rename_table", "Rename a table", "rename_table OLD NEW"),
            c(FID_DUPLICATE, "duplicate", "Copy a table (schema + rows)", "duplicate OLD NEW"),
            c(FID_ADD_COLUMN, "add_column", "Add a column", "add_column TABLE COL TYPE"),
            c(FID_TRANSFORM, "transform", "Alter columns (rename/drop/retype)",
              "transform TABLE [--rename A:B] [--drop C] [--type C:TYPE]"),
            c(FID_INDEX_FKS, "index_fks", "Index every foreign-key column", "index_fks [TABLE]"),
        ]
    }

    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        let t = tokens(&args);
        match id {
            FID_VIEWS => Ok(plain(spi::query(
                "SELECT view_name FROM duckdb_views() WHERE NOT internal ORDER BY view_name",
            )?)),

            FID_CREATE_TABLE => {
                // NAME COL:TYPE ... [--pk COL]
                if t.len() < 2 {
                    return Err("usage: .create_table NAME COL:TYPE [COL:TYPE ...] [--pk COL]".into());
                }
                let name = t[0];
                let mut pk: Option<&str> = None;
                let mut cols: Vec<std::string::String> = vec![];
                let mut i = 1;
                while i < t.len() {
                    if t[i] == "--pk" {
                        pk = t.get(i + 1).copied();
                        i += 2;
                        continue;
                    }
                    let (col, ty) = t[i].split_once(':').ok_or_else(|| {
                        format!("bad colspec '{}', expected COL:TYPE", t[i])
                    })?;
                    cols.push(format!("{} {}", quote_ident(col), map_type(ty)));
                    i += 1;
                }
                if cols.is_empty() {
                    return Err("create_table needs at least one COL:TYPE".into());
                }
                if let Some(pk) = pk {
                    cols.push(format!("PRIMARY KEY ({})", quote_ident(pk)));
                }
                run(
                    &format!("CREATE TABLE {} ({})", quote_ident(name), cols.join(", ")),
                    format!("created table {name}"),
                )
            }

            FID_CREATE_INDEX => {
                if t.len() < 2 {
                    return Err("usage: .create_index TABLE COL [COL ...]".into());
                }
                let table = t[0];
                let cols: Vec<std::string::String> = t[1..].iter().map(|c| quote_ident(c)).collect();
                let idx = format!("idx_{}_{}", table, t[1..].join("_"));
                run(
                    &format!("CREATE INDEX {} ON {} ({})",
                             quote_ident(&idx), quote_ident(table), cols.join(", ")),
                    format!("created index {idx}"),
                )
            }

            FID_CREATE_VIEW => {
                let name = t.first().ok_or("usage: .create_view NAME SELECT ...")?;
                let select = args.trim().strip_prefix(name).unwrap_or("").trim();
                if select.is_empty() {
                    return Err("usage: .create_view NAME SELECT ...".into());
                }
                run(
                    &format!("CREATE VIEW {} AS {}", quote_ident(name), select),
                    format!("created view {name}"),
                )
            }

            FID_DROP_TABLE | FID_DROP_VIEW => {
                let name = t.first().ok_or("usage: .drop_table NAME [--ignore]")?;
                let ignore = t.iter().any(|x| *x == "--ignore");
                let kind = if id == FID_DROP_TABLE { "TABLE" } else { "VIEW" };
                let ifx = if ignore { "IF EXISTS " } else { "" };
                run(
                    &format!("DROP {kind} {ifx}{}", quote_ident(name)),
                    format!("dropped {} {name}", kind.to_lowercase()),
                )
            }

            FID_RENAME_TABLE => {
                if t.len() != 2 {
                    return Err("usage: .rename_table OLD NEW".into());
                }
                run(
                    &format!("ALTER TABLE {} RENAME TO {}", quote_ident(t[0]), quote_ident(t[1])),
                    format!("renamed {} -> {}", t[0], t[1]),
                )
            }

            FID_DUPLICATE => {
                if t.len() != 2 {
                    return Err("usage: .duplicate OLD NEW".into());
                }
                run(
                    &format!("CREATE TABLE {} AS SELECT * FROM {}",
                             quote_ident(t[1]), quote_ident(t[0])),
                    format!("duplicated {} -> {}", t[0], t[1]),
                )
            }

            FID_ADD_COLUMN => {
                if t.len() != 3 {
                    return Err("usage: .add_column TABLE COL TYPE".into());
                }
                run(
                    &format!("ALTER TABLE {} ADD COLUMN {} {}",
                             quote_ident(t[0]), quote_ident(t[1]), map_type(t[2])),
                    format!("added column {} {} to {}", t[1], map_type(t[2]), t[0]),
                )
            }

            FID_TRANSFORM => {
                // TABLE [--rename A:B] [--drop C] [--type C:TYPE], applied in order.
                let table = t.first().ok_or(
                    "usage: .transform TABLE [--rename A:B] [--drop C] [--type C:TYPE]",
                )?;
                let qtable = quote_ident(table);
                let mut applied: Vec<std::string::String> = vec![];
                let mut i = 1;
                while i < t.len() {
                    let flag = t[i];
                    let val = t.get(i + 1).copied().ok_or_else(|| format!("{flag} needs an argument"))?;
                    let sql = match flag {
                        "--rename" => {
                            let (a, b) = val.split_once(':').ok_or("--rename expects A:B")?;
                            applied.push(format!("renamed {a} -> {b}"));
                            format!("ALTER TABLE {qtable} RENAME COLUMN {} TO {}",
                                    quote_ident(a), quote_ident(b))
                        }
                        "--drop" => {
                            applied.push(format!("dropped {val}"));
                            format!("ALTER TABLE {qtable} DROP COLUMN {}", quote_ident(val))
                        }
                        "--type" => {
                            let (c, ty) = val.split_once(':').ok_or("--type expects COL:TYPE")?;
                            applied.push(format!("retyped {c} -> {}", map_type(ty)));
                            format!("ALTER TABLE {qtable} ALTER COLUMN {} TYPE {}",
                                    quote_ident(c), map_type(ty))
                        }
                        other => return Err(format!("unknown transform flag {other}")),
                    };
                    spi::query(&sql)?;
                    i += 2;
                }
                if applied.is_empty() {
                    return Err("transform needs at least one --rename/--drop/--type".into());
                }
                Ok(note(format!("transformed {table}: {}", applied.join(", "))))
            }

            FID_INDEX_FKS => {
                // One index per FK column, from duckdb_constraints().
                let filter = match t.first() {
                    Some(tbl) => format!(" AND table_name = '{}'", tbl.replace('\'', "''")),
                    None => std::string::String::new(),
                };
                let rows = spi::query(&format!(
                    "SELECT table_name, unnest(constraint_column_names) AS col \
                     FROM duckdb_constraints() \
                     WHERE constraint_type = 'FOREIGN KEY'{filter} ORDER BY table_name, col"
                ))?;
                let mut made: Vec<std::string::String> = vec![];
                for line in rows.lines().filter(|l| !l.trim().is_empty()) {
                    let mut parts = line.split('\t');
                    let (table, col) = match (parts.next(), parts.next()) {
                        (Some(t), Some(c)) => (t, c),
                        _ => continue,
                    };
                    let idx = format!("fk_{table}_{col}");
                    spi::query(&format!(
                        "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                        quote_ident(&idx), quote_ident(table), quote_ident(col)
                    ))?;
                    made.push(idx);
                }
                Ok(note(if made.is_empty() {
                    "no foreign-key columns found".into()
                } else {
                    format!("created {} index(es): {}", made.len(), made.join(", "))
                }))
            }

            other => Err(format!("duckdb-utils-schema: unknown command id {other}")),
        }
    }
}
export!(Component);
