//! textlines: split text into rows as a DuckDB table function.
//!   split_lines(text VARCHAR) -> table(line_no BIGINT, line VARCHAR)
//! One row per line; splits on '\n' and handles '\r\n'; a trailing empty
//! line (text ending in a newline) is dropped. line_no is 1-based.
//! Empty input -> 0 rows. NULL input -> 0 rows. Never panics.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_table_function()?;
        Ok(types::Loadresult {
            name: "textlines".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }

    fn reconfigure(_keys: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }

    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        _handle: u32,
        _rows: Vec<Vec<types::Duckvalue>>,
        _ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "textlines has no scalar functions".into(),
        ))
    }

    fn call_scalar(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "textlines has no scalar functions".into(),
        ))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let handler = table_handlers()
            .lock()
            .expect("table handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        match handler {
            TableHandler::SplitLines => {
                // Accept a single VARCHAR arg. NULL -> 0 rows.
                let text = match args.as_slice() {
                    [types::Duckvalue::Text(t)] => t.as_str(),
                    [types::Duckvalue::Null] => return Ok(Vec::new()),
                    _ => {
                        return Err(types::Duckerror::Invalidargument(
                            "split_lines expects a single VARCHAR argument".into(),
                        ))
                    }
                };

                if text.is_empty() {
                    return Ok(Vec::new());
                }

                // Split on '\n'; strip a trailing '\r' so "\r\n" becomes a clean
                // boundary. Drop a single trailing empty line (text ending in \n).
                let mut parts: Vec<&str> = text.split('\n').collect();
                if let Some(last) = parts.last() {
                    if last.is_empty() {
                        parts.pop();
                    }
                }

                let mut rows = Vec::with_capacity(parts.len());
                for (i, part) in parts.into_iter().enumerate() {
                    let line = part.strip_suffix('\r').unwrap_or(part);
                    rows.push(vec![
                        types::Duckvalue::Int64((i as i64) + 1),
                        types::Duckvalue::Text(line.into()),
                    ]);
                }
                Ok(rows)
            }
        }
    }

    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "textlines has no aggregate functions".into(),
        ))
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "textlines has no pragma callbacks".into(),
        ))
    }

    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "textlines has no cast callbacks".into(),
        ))
    }
}

export!(Extension);

fn register_table_function() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose table capability".into()))?;

    let registry = match capability {
        runtime::Capability::Table(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "table capability returned unexpected variant".into(),
            ))
        }
    };

    let handle = NEXT_TABLE_HANDLE.fetch_add(1, Ordering::Relaxed);
    table_handlers()
        .lock()
        .expect("table handler mutex poisoned")
        .insert(handle, TableHandler::SplitLines);

    let callback = runtime::TableCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("text".into()),
        logical: types::Logicaltype::Text,
    }];
    let columns = vec![
        types::Columndef {
            name: "line_no".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "line".into(),
            logical: types::Logicaltype::Text,
        },
    ];
    let opts = runtime::Extopts {
        description: Some("Splits text into one row per line (1-based line_no)".into()),
        tags: vec!["text".into()],
    };

    registry.register("split_lines", &args, &columns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum TableHandler {
    SplitLines,
}

static NEXT_TABLE_HANDLE: AtomicU32 = AtomicU32::new(1);
static TABLE_HANDLERS: OnceLock<Mutex<HashMap<u32, TableHandler>>> = OnceLock::new();

fn table_handlers() -> &'static Mutex<HashMap<u32, TableHandler>> {
    TABLE_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
