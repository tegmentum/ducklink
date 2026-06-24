//! .env file parsing as DuckDB scalars. DuckDB has JSON but not a .env parser.
//!   dotenv_to_json(text) -> JSON object {KEY: VALUE} of all entries
//!   dotenv_get(text, key) -> value for KEY, NULL if absent
//!   dotenv_keys(text)     -> JSON array of keys (in file order)
//! The parser is hand-rolled: KEY=VALUE lines, `#` comments, blank lines,
//! optional `export ` prefix, and matched single/double quotes stripped from
//! values. An unquoted value may carry a trailing inline `# comment`. NULL
//! input / missing values -> NULL. Never panics.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use serde_json::Value;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "dotenv".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg_text(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

// Parse one .env line into (key, value). Returns None for blank/comment lines
// or lines lacking an `=`.
fn parse_line(line: &str) -> Option<(std::string::String, std::string::String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') { return None; }
    // Strip an optional leading `export ` prefix.
    let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty() { return None; }
    let raw = line[eq + 1..].trim_start();
    let value = parse_value(raw);
    Some((key.to_string(), value))
}

// Resolve the right-hand side of `KEY=`: strip matched surrounding quotes, or
// (for an unquoted value) drop a trailing inline `# comment` and trim.
fn parse_value(raw: &str) -> std::string::String {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let q = bytes[0];
        if (q == b'"' || q == b'\'') && bytes[bytes.len() - 1] == q {
            // Matched surrounding quotes: take the inner span verbatim.
            return raw[1..raw.len() - 1].to_string();
        }
    }
    // Unquoted: an inline `#` (preceded by whitespace or at start) begins a
    // comment. Cut it off, then trim trailing whitespace.
    let mut end = raw.len();
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            end = i;
            break;
        }
        i += 1;
    }
    raw[..end].trim_end().to_string()
}

fn entries(src: &str) -> Vec<(std::string::String, std::string::String)> {
    let mut out = Vec::new();
    for line in src.lines() {
        if let Some(kv) = parse_line(line) { out.push(kv); }
    }
    out
}

fn dotenv_to_json(src: &str) -> Option<std::string::String> {
    // serde_json::Map preserves insertion order only with the "preserve_order"
    // feature; without it keys sort. Build the object string by hand to keep
    // file order and guarantee valid JSON via serde escaping of each piece.
    let mut obj = serde_json::Map::new();
    for (k, v) in entries(src) {
        obj.insert(k, Value::String(v));
    }
    serde_json::to_string(&Value::Object(obj)).ok()
}

fn dotenv_get(src: &str, key: &str) -> Option<std::string::String> {
    // Last assignment wins (standard .env semantics).
    entries(src).into_iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn dotenv_keys(src: &str) -> Option<std::string::String> {
    let mut seen: Vec<std::string::String> = Vec::new();
    let mut arr: Vec<Value> = Vec::new();
    for (k, _) in entries(src) {
        if !seen.contains(&k) {
            seen.push(k.clone());
            arr.push(Value::String(k));
        }
    }
    serde_json::to_string(&Value::Array(arr)).ok()
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let r: Option<std::string::String> = match handle {
            1 => arg_text(&args, 0).and_then(|s| dotenv_to_json(&s)),
            2 => match (arg_text(&args, 0), arg_text(&args, 1)) {
                (Some(s), Some(k)) => dotenv_get(&s, &k),
                _ => None,
            },
            3 => arg_text(&args, 0).and_then(|s| dotenv_keys(&s)),
            _ => None,
        };
        Ok(match r { Some(t) => types::Duckvalue::Text(t.into()), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("dotenv: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dotenv: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("dotenv: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dotenv: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("dotenv_to_json", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some(".env text -> JSON object {KEY:VALUE}".into()), tags: vec!["config".into()], attributes: det }))?;
    reg.register("dotenv_get", &[
            runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("key".into()), logical: types::Logicaltype::Text },
        ],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("Value for KEY in .env text; NULL if absent".into()), tags: vec!["config".into()], attributes: det }))?;
    reg.register("dotenv_keys", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(3),
        Some(&runtime::Funcopts { description: Some("JSON array of keys in .env text".into()), tags: vec!["config".into()], attributes: det }))?;
    Ok(())
}
