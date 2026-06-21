//! Ordinal number formatting as a DuckDB scalar (hand-rolled):
//!   ordinal(n) -> text ("1st", "2nd", "3rd", "11th", "21st", "-4th").
//!   NULL -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ordinal".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn ordinal(n: i64) -> std::string::String {
    let last2 = (n.unsigned_abs() % 100) as u64;
    let suffix = if (11..=13).contains(&last2) { "th" }
        else { match last2 % 10 { 1 => "st", 2 => "nd", 3 => "rd", _ => "th" } };
    format!("{}{}", n, suffix)
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
        let n = match args.first() {
            Some(types::Duckvalue::Int64(n)) => *n,
            Some(types::Duckvalue::Uint64(n)) => match i64::try_from(*n) { Ok(v) => v, Err(_) => return Ok(types::Duckvalue::Null) },
            _ => return Ok(types::Duckvalue::Null),
        };
        Ok(types::Duckvalue::Text(ordinal(n).into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ordinal: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ordinal: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ordinal: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ordinal: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("ordinal", &[runtime::Funcarg { name: Some("n".into()), logical: types::Logicaltype::Int64 }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("ordinal suffix".into()), tags: vec!["text".into()], attributes: det }))?;
    Ok(())
}
