//! Apple property-list (plist) parsing as DuckDB scalars.
//!
//! - `plist_to_json(data)` parses an XML or binary plist into JSON. The input
//!   may arrive as TEXT (XML plist) or BLOB (binary or XML plist); the plist
//!   crate auto-detects the format from the byte stream.
//! - `plist_get(data, key)` returns the value of a top-level dict key rendered
//!   as text, or NULL if the input is not a dict or the key is absent.
//!
//! Any parse error yields NULL; the guest never panics.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "plist".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

/// Coerce the first argument into raw bytes, accepting either TEXT or BLOB.
fn bytes(args: &[types::Duckvalue]) -> Option<std::vec::Vec<u8>> {
    match args.first() {
        Some(types::Duckvalue::Text(s)) => Some(s.clone().into_bytes()),
        Some(types::Duckvalue::Blob(b)) => Some(b.clone().to_vec()),
        _ => None,
    }
}

fn text_arg(args: &[types::Duckvalue], idx: usize) -> Option<std::string::String> {
    match args.get(idx) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone().into()),
        _ => None,
    }
}

/// Parse plist bytes (XML or binary, auto-detected) into a `plist::Value`.
fn parse(data: &[u8]) -> Option<plist::Value> {
    plist::Value::from_reader(std::io::Cursor::new(data)).ok()
}

/// Convert a `plist::Value` into a `serde_json::Value`. Data is base64-encoded,
/// dates are rendered as RFC3339 strings, both kept as JSON strings.
fn to_json(v: &plist::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        plist::Value::Boolean(b) => J::Bool(*b),
        plist::Value::Integer(i) => {
            if let Some(n) = i.as_signed() {
                J::Number(n.into())
            } else if let Some(n) = i.as_unsigned() {
                J::Number(n.into())
            } else {
                J::Null
            }
        }
        plist::Value::Real(r) => serde_json::Number::from_f64(*r)
            .map(J::Number)
            .unwrap_or(J::Null),
        plist::Value::String(s) => J::String(s.clone()),
        plist::Value::Uid(u) => J::Number(u.get().into()),
        plist::Value::Data(d) => J::String(b64_encode(d)),
        plist::Value::Date(d) => J::String(d.to_xml_format()),
        plist::Value::Array(a) => J::Array(a.iter().map(to_json).collect()),
        plist::Value::Dictionary(m) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in m.iter() {
                obj.insert(k.clone(), to_json(val));
            }
            J::Object(obj)
        }
        _ => J::Null,
    }
}

/// Minimal standard base64 encoder (no external dep).
fn b64_encode(input: &[u8]) -> std::string::String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = std::string::String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Render a single plist value as plain text for `plist_get`. Strings come back
/// unquoted; containers fall back to their JSON form.
fn value_to_text(v: &plist::Value) -> std::string::String {
    match v {
        plist::Value::String(s) => s.clone(),
        plist::Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
        plist::Value::Integer(i) => {
            if let Some(n) = i.as_signed() {
                n.to_string()
            } else if let Some(n) = i.as_unsigned() {
                n.to_string()
            } else {
                std::string::String::new()
            }
        }
        plist::Value::Real(r) => r.to_string(),
        plist::Value::Uid(u) => u.get().to_string(),
        plist::Value::Date(d) => d.to_xml_format(),
        plist::Value::Data(d) => b64_encode(d),
        other => serde_json::to_string(&to_json(other)).unwrap_or_default(),
    }
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
        let data = match bytes(&args) { Some(d) => d, None => return Ok(types::Duckvalue::Null) };
        let v = match parse(&data) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        match handle {
            // plist_to_json(data)
            1 => {
                let json = to_json(&v);
                match serde_json::to_string(&json) {
                    Ok(s) => Ok(types::Duckvalue::Text(s.into())),
                    Err(_) => Ok(types::Duckvalue::Null),
                }
            }
            // plist_get(data, key)
            2 => {
                let key = match text_arg(&args, 1) { Some(k) => k, None => return Ok(types::Duckvalue::Null) };
                match v.as_dictionary().and_then(|d| d.get(&key)) {
                    Some(val) => Ok(types::Duckvalue::Text(value_to_text(val).into())),
                    None => Ok(types::Duckvalue::Null),
                }
            }
            _ => Ok(types::Duckvalue::Null),
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("plist: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("plist: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("plist: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("plist: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register(
        "plist_to_json",
        &[runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("plist (XML/binary) -> JSON".into()), tags: vec!["data-types".into()], attributes: det }),
    )?;
    reg.register(
        "plist_get",
        &[
            runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("key".into()), logical: types::Logicaltype::Text },
        ],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("plist top-level dict value by key".into()), tags: vec!["data-types".into()], attributes: det }),
    )?;
    Ok(())
}
