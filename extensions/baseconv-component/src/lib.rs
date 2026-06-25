//! Arbitrary-radix string -> integer as a DuckDB scalar (hand-rolled):
//!   from_base(text, base) -> bigint, base 2..=36. This is the inverse of
//!   DuckDB's built-in `to_base(n, radix)` (which DuckDB provides but offers no
//!   decode for). Case-insensitive; NULL / unparseable / bad base -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "baseconv".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) { Some(types::Duckvalue::Int64(n)) => Some(*n), Some(types::Duckvalue::Uint64(n)) => i64::try_from(*n).ok(), _ => None }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(_handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let s = text_arg(&args, 0); let base = i64_arg(&args, 1);
        Ok(match (s, base) {
            (Some(s), Some(b)) if (2..=36).contains(&b) =>
                match i64::from_str_radix(s.trim(), b as u32) { Ok(n) => types::Duckvalue::Int64(n), Err(_) => types::Duckvalue::Null },
            _ => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("baseconv: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("baseconv: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("baseconv: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("baseconv: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("from_base", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("base".into()), logical: types::Logicaltype::Int64 }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("base-N text -> integer (inverse of to_base)".into()), tags: vec!["base".into()], attributes: det }))?;
    Ok(())
}
