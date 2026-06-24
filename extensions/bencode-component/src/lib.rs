//! BitTorrent bencode as DuckDB scalars (via `serde_bencode`):
//!   bencode_to_json(data BLOB) -> VARCHAR (JSON value; NULL on decode error),
//!   bencode_is_valid(data BLOB) -> BOOLEAN.
//! Byte-strings decode to UTF-8 text where possible, otherwise to lowercase
//! hex. Never panics; bad input -> NULL / false.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use serde_bencode::value::Value as Ben;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "bencode".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn blob_arg(args: &[types::Duckvalue], i: usize) -> Option<std::vec::Vec<u8>> {
    match args.get(i) {
        Some(types::Duckvalue::Blob(b)) => Some(b.clone()),
        // Allow TEXT inputs too, so string literals work without an explicit cast.
        Some(types::Duckvalue::Text(s)) => Some(s.clone().into_bytes()),
        _ => None,
    }
}

/// Render bytes as a JSON string token: UTF-8 text when valid, else hex.
fn bytes_to_json_string(bytes: &[u8], out: &mut std::string::String) {
    match std::str::from_utf8(bytes) {
        Ok(s) => escape_json_string(s, out),
        Err(_) => {
            let mut hex = std::string::String::with_capacity(bytes.len() * 2);
            for b in bytes {
                hex.push_str(&format!("{:02x}", b));
            }
            escape_json_string(&hex, out);
        }
    }
}

fn escape_json_string(s: &str, out: &mut std::string::String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialize a bencode value to JSON text. Dict keys are byte-strings rendered
/// the same way as scalar byte-strings (UTF-8 text or hex).
fn ben_to_json(v: &Ben, out: &mut std::string::String) {
    match v {
        Ben::Int(i) => out.push_str(&i.to_string()),
        Ben::Bytes(b) => bytes_to_json_string(b, out),
        Ben::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push(','); }
                ben_to_json(item, out);
            }
            out.push(']');
        }
        Ben::Dict(map) => {
            // serde_bencode's Dict is a HashMap, so iteration order is not
            // stable. Bencode dicts are canonically sorted by raw byte key;
            // sort here so JSON output is deterministic.
            let mut entries: std::vec::Vec<(&std::vec::Vec<u8>, &Ben)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (k, val)) in entries.into_iter().enumerate() {
                if i > 0 { out.push(','); }
                bytes_to_json_string(k, out);
                out.push(':');
                ben_to_json(val, out);
            }
            out.push('}');
        }
    }
}

fn decode_to_json(data: &[u8]) -> Option<std::string::String> {
    let v: Ben = serde_bencode::from_bytes(data).ok()?;
    let mut out = std::string::String::new();
    ben_to_json(&v, &mut out);
    Some(out)
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            B::ToJson => match blob_arg(&args, 0) {
                Some(bytes) => match decode_to_json(&bytes) {
                    Some(json) => types::Duckvalue::Text(json.into()),
                    None => types::Duckvalue::Null,
                },
                None => types::Duckvalue::Null,
            },
            B::IsValid => match blob_arg(&args, 0) {
                Some(bytes) => types::Duckvalue::Boolean(
                    serde_bencode::from_bytes::<Ben>(&bytes).is_ok()
                ),
                None => types::Duckvalue::Boolean(false),
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("bencode: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bencode: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("bencode: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bencode: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, B::ToJson);
    reg.register("bencode_to_json",
        &[runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Blob }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("bencode BLOB -> JSON text".into()), tags: vec!["encoding".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, B::IsValid);
    reg.register("bencode_is_valid",
        &[runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Blob }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("is the BLOB valid bencode?".into()), tags: vec!["encoding".into()], attributes: det }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum B { ToJson, IsValid }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, B>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, B>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
