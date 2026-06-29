//! Read Garmin/ANT .FIT activity files as a DuckDB table function over an
//! in-memory BLOB.
//!
//!   read_fit(data BLOB)
//!     -> table(record_no BIGINT, kind VARCHAR, field VARCHAR, value VARCHAR)
//!
//! A .FIT file is a stream of "data messages" of many different kinds (file_id,
//! record, session, lap, event, ...), and each kind carries its own set of
//! fields. Because DuckDB table-function output columns must be fixed at bind
//! time, the messages are returned in melted / "long" form: one row per data
//! field, across all messages.
//!
//!   record_no : 1-indexed position of the data message in the file
//!   kind      : the message kind (e.g. 'record', 'session', 'lap', 'file_id')
//!   field     : the FIT-profile field name (e.g. 'heart_rate', 'timestamp')
//!   value     : the field value rendered as text
//!
//! Ordering is deterministic: messages are emitted in file order, and the
//! fields within a message are emitted in their parsed (file) order. A
//! malformed/empty blob yields zero rows -- never a panic.
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
        register_read_fit()?;
        Ok(types::Loadresult {
            name: "fit".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

impl callback_dispatch::Guest for Extension {
    // major-4 columnar hot path: fit is table-only, so the three columnar
    // methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("fit: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // single registered table fn; any handle maps to read_fit
        let _ = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        let bytes: std::vec::Vec<u8> = match args.into_iter().next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            // accept TEXT too, so `read_fit(<varchar>)` degrades gracefully
            Some(types::Duckvalue::Text(s)) => s.into_bytes(),
            Some(types::Duckvalue::Null) | None => return Ok(Vec::new().into()),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "read_fit expects a single BLOB argument".into(),
                ))
            }
        };

        Ok(melt(&bytes).into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("fit: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("fit: no casts".into()))
    }
}

export!(Extension);

/// Parse the .FIT bytes and emit melted rows. Returns an empty result on any
/// malformed input rather than panicking or erroring.
fn melt(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::new();

    // from_bytes returns Err on a malformed/truncated/empty stream; treat as 0 rows.
    let records = match fitparser::from_bytes(bytes) {
        Ok(r) => r,
        Err(_) => return out,
    };

    for (idx, record) in records.into_iter().enumerate() {
        let record_no = (idx as i64) + 1;
        let kind = record.kind().to_string();
        // fields() preserves the parsed (file) order -- deterministic.
        for field in record.fields() {
            let value = render(field.value());
            let value_val = match value {
                Some(s) if !s.is_empty() => types::Duckvalue::Text(s.into()),
                _ => types::Duckvalue::Null,
            };
            out.push(vec![
                types::Duckvalue::Int64(record_no),
                types::Duckvalue::Text(kind.clone().into()),
                types::Duckvalue::Text(field.name().into()),
                value_val,
            ]);
        }
    }

    out
}

/// Render a FIT field value as text. `Invalid` -> None (becomes NULL).
fn render(value: &fitparser::Value) -> Option<std::string::String> {
    match value {
        fitparser::Value::Invalid => None,
        // Value implements Display for every variant (snake_case kinds, plain
        // scalars, debug-formatted arrays) -- reuse it for a stable rendering.
        other => Some(other.to_string()),
    }
}

fn register_read_fit() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::ReadFit);

    let args = vec![runtime::Funcarg {
        name: Some("data".into()),
        logical: types::Logicaltype::Blob,
    }];
    let columns = vec![
        types::Columndef {
            name: "record_no".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "kind".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "field".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "value".into(),
            logical: types::Logicaltype::Text,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Read Garmin/ANT .FIT bytes into melted (record_no, kind, field, value) rows".into(),
        ),
        tags: vec!["fit".into(), "garmin".into(), "ant".into()],
    };
    reg.register("read_fit", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    ReadFit,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
