//! JSON <-> CBOR as DuckDB scalars (via `ciborium`, bridged through
//! serde_json::Value): cbor_from_json(json) -> hex text of the CBOR bytes,
//! cbor_to_json(hex) -> JSON text. Invalid input -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "cbor".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn json_to_cbor_hex(json: &str) -> Option<std::string::String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut buf = std::vec::Vec::new();
    ciborium::into_writer(&v, &mut buf).ok()?;
    Some(hex::encode(buf))
}
fn cbor_hex_to_json(h: &str) -> Option<std::string::String> {
    let bytes = hex::decode(h.trim()).ok()?;
    let v: serde_json::Value = ciborium::from_reader(&bytes[..]).ok()?;
    serde_json::to_string(&v).ok()
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
        let r = if handle == 1 { json_to_cbor_hex(&s) } else { cbor_hex_to_json(&s) };
        Ok(match r { Some(t) => types::Duckvalue::Text(t.into()), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("cbor: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("cbor: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("cbor: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("cbor: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("cbor_from_json", &[runtime::Funcarg { name: Some("json".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("JSON -> CBOR hex".into()), tags: vec!["codec".into()], attributes: det }))?;
    reg.register("cbor_to_json", &[runtime::Funcarg { name: Some("hex".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("CBOR hex -> JSON".into()), tags: vec!["codec".into()], attributes: det }))?;
    Ok(())
}
