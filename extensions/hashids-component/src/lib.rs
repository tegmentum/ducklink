//! Hashids integer-id obfuscation as DuckDB scalars (via `harsh`):
//!   hashids_encode(number, salt) -> text (YouTube-style id),
//!   hashids_decode(text, salt) -> bigint (first value). NULL / undecodable -> NULL.
use harsh::Harsh;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "hashids".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn harsh_with(salt: &str) -> Option<Harsh> { Harsh::builder().salt(salt).build().ok() }
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        if handle == 1 {
            let n = match args.first() { Some(types::Duckvalue::Int64(n)) if *n >= 0 => *n as u64, Some(types::Duckvalue::Uint64(n)) => *n, _ => return Ok(types::Duckvalue::Null) };
            let salt = text_arg(&args, 1).unwrap_or_default();
            let harsh = match harsh_with(&salt) { Some(h) => h, None => return Ok(types::Duckvalue::Null) };
            let s = harsh.encode(&[n]);
            return Ok(if s.is_empty() { types::Duckvalue::Null } else { types::Duckvalue::Text(s.into()) });
        }
        let input = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let salt = text_arg(&args, 1).unwrap_or_default();
        let harsh = match harsh_with(&salt) { Some(h) => h, None => return Ok(types::Duckvalue::Null) };
        Ok(match harsh.decode(input.trim()).ok().and_then(|v| v.first().copied()) {
            Some(n) => types::Duckvalue::Int64(n as i64), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("hashids: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("hashids: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("hashids: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("hashids: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("hashids_encode", &[
        runtime::Funcarg { name: Some("number".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("salt".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("Hashids encode".into()), tags: vec!["id".into()], attributes: det }))?;
    reg.register("hashids_decode", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("salt".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("Hashids decode".into()), tags: vec!["id".into()], attributes: det }))?;
    Ok(())
}
