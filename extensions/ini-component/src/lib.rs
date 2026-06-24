//! INI / config-file parsing as DuckDB scalars (via `rust-ini`, crate `ini`,
//! bridged through serde_json). DuckDB has JSON but not INI.
//!   ini_to_json(ini) -> JSON object {section: {key: value}}
//!   ini_get(ini, section, key) -> value of section.key, NULL if absent
//!   ini_sections(ini) -> JSON array of section names
//! Keys outside any [section] (the "general" section) are placed under the
//! "" (empty-string) key. Invalid input / missing values -> NULL. Never panics.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use serde_json::{Map, Value};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ini".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg_text(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
// Parse INI into serde_json::Value (object of objects). General (no-section)
// keys go under "". Returns None on parse error.
fn parse(src: &str) -> Option<Value> {
    let conf = ini::Ini::load_from_str(src).ok()?;
    let mut root = Map::new();
    for (sec, props) in conf.iter() {
        // rust-ini always yields a general (no-section) bucket; skip it when it
        // holds no keys so empty INIs / fully-sectioned INIs stay clean.
        if sec.is_none() && props.is_empty() { continue; }
        let name = sec.unwrap_or("").to_string();
        let entry = root.entry(name).or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(m) = entry {
            for (k, v) in props.iter() {
                m.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
    }
    Some(Value::Object(root))
}
fn ini_to_json(src: &str) -> Option<std::string::String> {
    serde_json::to_string(&parse(src)?).ok()
}
fn ini_get(src: &str, section: &str, key: &str) -> Option<std::string::String> {
    let conf = ini::Ini::load_from_str(src).ok()?;
    let sec = if section.is_empty() { None } else { Some(section) };
    conf.get_from(sec, key).map(|s| s.to_string())
}
fn ini_sections(src: &str) -> Option<std::string::String> {
    let conf = ini::Ini::load_from_str(src).ok()?;
    let mut seen = Vec::new();
    let mut names: Vec<Value> = Vec::new();
    for (sec, props) in conf.iter() {
        if sec.is_none() && props.is_empty() { continue; }
        let name = sec.unwrap_or("").to_string();
        if !seen.contains(&name) {
            seen.push(name.clone());
            names.push(Value::String(name));
        }
    }
    serde_json::to_string(&Value::Array(names)).ok()
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
            1 => arg_text(&args, 0).and_then(|s| ini_to_json(&s)),
            2 => match (arg_text(&args, 0), arg_text(&args, 1), arg_text(&args, 2)) {
                (Some(s), Some(sec), Some(k)) => ini_get(&s, &sec, &k),
                _ => None,
            },
            3 => arg_text(&args, 0).and_then(|s| ini_sections(&s)),
            _ => None,
        };
        Ok(match r { Some(t) => types::Duckvalue::Text(t.into()), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ini: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ini: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ini: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ini: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("ini_to_json", &[runtime::Funcarg { name: Some("ini".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("INI -> JSON object {section:{key:value}}".into()), tags: vec!["config".into()], attributes: det }))?;
    reg.register("ini_get", &[
            runtime::Funcarg { name: Some("ini".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("section".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("key".into()), logical: types::Logicaltype::Text },
        ],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("Value of section.key in INI; NULL if absent".into()), tags: vec!["config".into()], attributes: det }))?;
    reg.register("ini_sections", &[runtime::Funcarg { name: Some("ini".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(3),
        Some(&runtime::Funcopts { description: Some("JSON array of section names in INI".into()), tags: vec!["config".into()], attributes: det }))?;
    Ok(())
}
