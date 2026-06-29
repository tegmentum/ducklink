//! Read dBASE/.dbf files as a DuckDB table function over an in-memory BLOB.
//!
//!   read_dbf(data BLOB) -> table(record_no BIGINT, field VARCHAR, value VARCHAR)
//!
//! Because DuckDB table-function output columns must be fixed at bind time (and
//! a .dbf can carry any field schema), the records are returned in melted /
//! "long" form: one row per field per record. `record_no` is 1-indexed, `field`
//! is the dBASE field name, `value` is the field rendered as text (NULL for an
//! empty/absent field). A malformed blob yields zero rows -- never a panic.
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use dbase::FieldValue;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_read_dbf()?;
        Ok(types::Loadresult {
            name: "dbf".into(),
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
    // major-4 columnar dispatch: dbf is table-only, so the columnar hot methods
    // are Unsupported stubs. The hand-written call_table below is unchanged.
    datalink_extcore::columnar_stub!();
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("dbf: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // single registered table fn; any handle maps to read_dbf
        let _ = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        let bytes: std::vec::Vec<u8> = match args.into_iter().next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            // accept TEXT too, so `read_dbf(<varchar>)` degrades gracefully
            Some(types::Duckvalue::Text(s)) => s.into_bytes(),
            Some(types::Duckvalue::Null) | None => return Ok(Vec::new().into()),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "read_dbf expects a single BLOB argument".into(),
                ))
            }
        };

        Ok(melt(&bytes).into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("dbf: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("dbf: no casts".into()))
    }
}

export!(Extension);

/// Read the .dbf bytes and emit melted rows. Returns an empty result on any
/// malformed input rather than panicking or erroring.
fn melt(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::new();

    let mut reader = match dbase::Reader::new(Cursor::new(bytes.to_vec())) {
        Ok(r) => r,
        Err(_) => return out,
    };

    // Declared field order is deterministic (dbase::Record is HashMap-backed,
    // so we drive ordering from the header's field list, not record iteration).
    let field_names: std::vec::Vec<String> =
        reader.fields().iter().map(|f| f.name().into()).collect();

    let records = match reader.read() {
        Ok(recs) => recs,
        Err(_) => return out,
    };

    for (idx, record) in records.into_iter().enumerate() {
        let record_no = (idx as i64) + 1;
        for name in &field_names {
            let value = match record.get(name) {
                Some(fv) => render(fv),
                None => None,
            };
            let value_val = match value {
                Some(s) if !s.is_empty() => types::Duckvalue::Text(s.into()),
                _ => types::Duckvalue::Null,
            };
            out.push(vec![
                types::Duckvalue::Int64(record_no),
                types::Duckvalue::Text(name.clone()),
                value_val,
            ]);
        }
    }

    out
}

/// Render a dBASE field value as text. Empty/None -> None (becomes NULL).
fn render(fv: &FieldValue) -> Option<std::string::String> {
    match fv {
        FieldValue::Character(opt) => opt.clone(),
        FieldValue::Numeric(opt) => opt.map(|v| fmt_f64(v)),
        FieldValue::Float(opt) => opt.map(|v| fmt_f64(v as f64)),
        FieldValue::Logical(opt) => opt.map(|b| if b { "true".into() } else { "false".into() }),
        FieldValue::Date(opt) => opt.map(|d| d.to_string()),
        FieldValue::Integer(i) => Some(i.to_string()),
        FieldValue::Currency(c) => Some(fmt_f64(*c)),
        FieldValue::Double(d) => Some(fmt_f64(*d)),
        FieldValue::DateTime(dt) => Some(format!("{:?}", dt)),
        FieldValue::Memo(s) => Some(s.clone()),
    }
}

/// Render an f64 without a trailing ".0" for whole numbers, so 30.0 -> "30".
fn fmt_f64(v: f64) -> std::string::String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

fn register_read_dbf() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::ReadDbf);

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
            name: "field".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "value".into(),
            logical: types::Logicaltype::Text,
        },
    ];
    let opts = runtime::Extopts {
        description: Some("Read dBASE/.dbf bytes into melted (record_no, field, value) rows".into()),
        tags: vec!["dbf".into(), "dbase".into()],
    };
    reg.register("read_dbf", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    ReadDbf,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
