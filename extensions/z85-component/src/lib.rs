//! Z85 (ZeroMQ Base85) as DuckDB scalars (via `z85`): z85_encode(hex) -> text,
//! z85_decode(text) -> hex. Invalid input -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "z85".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
const HEX: &[u8] = b"0123456789abcdef";
fn hex_encode(b: &[u8]) -> std::string::String {
    let mut o = std::string::String::with_capacity(b.len() * 2);
    for &x in b { o.push(HEX[(x >> 4) as usize] as char); o.push(HEX[(x & 0xf) as usize] as char); }
    o
}
fn hex_decode(s: &str) -> Option<std::vec::Vec<u8>> {
    let s = s.trim(); if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
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
        Ok(if handle == 1 {
            match hex_decode(&s) { Some(b) if b.len() % 4 == 0 => types::Duckvalue::Text(z85::encode(&b).into()), _ => types::Duckvalue::Null }
        } else {
            match z85::decode(s.trim()) { Ok(b) => types::Duckvalue::Text(hex_encode(&b).into()), Err(_) => types::Duckvalue::Null }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("z85: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("z85: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("z85: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("z85: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("z85_encode", &[runtime::Funcarg { name: Some("hex".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("Z85 encode".into()), tags: vec!["encoding".into()], attributes: det }))?;
    reg.register("z85_decode", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("Z85 decode".into()), tags: vec!["encoding".into()], attributes: det }))?;
    Ok(())
}
