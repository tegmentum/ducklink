//! Read WARC (Web ARChive, ISO 28500) files as a DuckDB table function over an
//! in-memory BLOB.
//!
//!   read_warc(data BLOB) -> table(
//!       record_no      BIGINT,   -- 1-indexed record ordinal
//!       warc_type      VARCHAR,  -- WARC-Type header (response, request, ...)
//!       target_uri     VARCHAR,  -- WARC-Target-URI header
//!       content_type   VARCHAR,  -- Content-Type header
//!       content_length BIGINT)   -- Content-Length header
//!
//! One row per WARC record. Only header fields are emitted (the record bodies
//! are parsed/skipped, never returned) so the result rows stay small. Absent
//! header fields become NULL. A malformed or empty blob yields zero rows --
//! never a panic.
use std::collections::HashMap;
use std::io::BufReader;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use warc::{WarcHeader, WarcReader};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_read_warc()?;
        Ok(types::Loadresult {
            name: "warc".into(),
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
    // major-4 columnar dispatch: warc is a table-only component, so the three
    // columnar hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("warc: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // single registered table fn; any handle maps to read_warc
        let _ = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        let bytes: std::vec::Vec<u8> = match args.into_iter().next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            // accept TEXT too, so `read_warc(<varchar>)` degrades gracefully
            Some(types::Duckvalue::Text(s)) => s.into_bytes(),
            Some(types::Duckvalue::Null) | None => return Ok(Vec::new().into()),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "read_warc expects a single BLOB argument".into(),
                ))
            }
        };

        Ok(parse(&bytes).into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("warc: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("warc: no casts".into()))
    }
}

export!(Extension);

/// Parse the WARC bytes and emit one row per record (header fields only).
/// Returns an empty result on any malformed input rather than panicking or
/// erroring. Iteration stops at the first malformed record (we keep whatever
/// well-formed records preceded it).
fn parse(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::new();

    // `iter_raw_records` only does header well-formedness checks and skips the
    // bodies for us -- exactly the "header fields only" shape we want, and it
    // tolerates records that lack the fields `iter_records` insists on.
    let reader = WarcReader::new(BufReader::new(bytes));

    let mut record_no: i64 = 0;
    for item in reader.iter_raw_records() {
        let (header, _body) = match item {
            Ok(r) => r,
            // stop at the first malformed/truncated record; keep prior rows
            Err(_) => break,
        };
        record_no += 1;

        let map: &HashMap<WarcHeader, std::vec::Vec<u8>> = header.as_ref();

        out.push(vec![
            types::Duckvalue::Int64(record_no),
            text_field(map, &WarcHeader::WarcType),
            text_field(map, &WarcHeader::TargetURI),
            text_field(map, &WarcHeader::ContentType),
            int_field(map, &WarcHeader::ContentLength),
        ]);
    }

    out
}

/// Render a header value as text. Absent or non-UTF-8 -> Null.
fn text_field(
    map: &HashMap<WarcHeader, std::vec::Vec<u8>>,
    key: &WarcHeader,
) -> types::Duckvalue {
    match map.get(key) {
        Some(raw) => match std::str::from_utf8(raw) {
            Ok(s) if !s.trim().is_empty() => types::Duckvalue::Text(s.trim().into()),
            _ => types::Duckvalue::Null,
        },
        None => types::Duckvalue::Null,
    }
}

/// Render a header value as an i64. Absent or unparsable -> Null.
fn int_field(
    map: &HashMap<WarcHeader, std::vec::Vec<u8>>,
    key: &WarcHeader,
) -> types::Duckvalue {
    match map.get(key).and_then(|raw| std::str::from_utf8(raw).ok()) {
        Some(s) => match s.trim().parse::<i64>() {
            Ok(v) => types::Duckvalue::Int64(v),
            Err(_) => types::Duckvalue::Null,
        },
        None => types::Duckvalue::Null,
    }
}

fn register_read_warc() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::ReadWarc);

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
            name: "warc_type".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "target_uri".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "content_type".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "content_length".into(),
            logical: types::Logicaltype::Int64,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Read WARC web-archive bytes into one (record_no, warc_type, target_uri, \
             content_type, content_length) row per record"
                .into(),
        ),
        tags: vec!["warc".into(), "web-archive".into()],
    };
    reg.register(
        "read_warc",
        &args,
        &columns,
        runtime::TableCallback::new(h),
        Some(&opts),
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    ReadWarc,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
