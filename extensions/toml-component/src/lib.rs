//! TOML <-> JSON as DuckDB scalars (via `toml`, bridged through
//! serde_json::Value): toml_to_json(toml) -> json, json_to_toml(json) -> toml.
//! json_to_toml requires a JSON object at top level. Invalid input -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "toml".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
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
        let s = match text(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let r: Option<std::string::String> = if handle == 1 {
            toml::from_str::<serde_json::Value>(&s).ok().and_then(|v| serde_json::to_string(&v).ok())
        } else {
            serde_json::from_str::<serde_json::Value>(&s).ok().and_then(|v| toml::to_string(&v).ok())
        };
        Ok(match r { Some(t) => types::Duckvalue::Text(t.into()), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("toml: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("toml: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("toml: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("toml: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("toml_to_json", &[runtime::Funcarg { name: Some("toml".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("TOML -> JSON".into()), tags: vec!["config".into()], attributes: det }))?;
    reg.register("json_to_toml", &[runtime::Funcarg { name: Some("json".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("JSON -> TOML".into()), tags: vec!["config".into()], attributes: det }))?;
    Ok(())
}
