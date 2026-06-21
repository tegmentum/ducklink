//! Geographic coordinate conversion as DuckDB scalars (hand-rolled):
//!   dms_to_decimal(deg, min, sec) -> double (sign follows deg),
//!   decimal_to_dms(decimal) -> text ("D°M'S.s\""). NULL arg -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "dms".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn f(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) { Some(types::Duckvalue::Float64(v)) => Some(*v), Some(types::Duckvalue::Int64(v)) => Some(*v as f64), _ => None }
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
        if handle == 1 {
            let (d, m, s) = match (f(&args, 0), f(&args, 1), f(&args, 2)) { (Some(a), Some(b), Some(c)) => (a, b, c), _ => return Ok(types::Duckvalue::Null) };
            let sign = if d.is_sign_negative() { -1.0 } else { 1.0 };
            return Ok(types::Duckvalue::Float64(sign * (d.abs() + m / 60.0 + s / 3600.0)));
        }
        let dec = match f(&args, 0) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        let neg = dec.is_sign_negative();
        let a = dec.abs();
        let deg = a.trunc();
        let rem = (a - deg) * 60.0;
        let min = rem.trunc();
        let sec = (rem - min) * 60.0;
        Ok(types::Duckvalue::Text(format!("{}{}\u{00b0}{}'{:.1}\"", if neg { "-" } else { "" }, deg as i64, min as i64, sec).into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("dms: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dms: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("dms: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dms: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("dms_to_decimal", &[
        runtime::Funcarg { name: Some("deg".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("min".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("sec".into()), logical: types::Logicaltype::Float64 }],
        types::Logicaltype::Float64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("DMS -> decimal degrees".into()), tags: vec!["geo".into()], attributes: det }))?;
    reg.register("decimal_to_dms", &[runtime::Funcarg { name: Some("decimal".into()), logical: types::Logicaltype::Float64 }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("decimal degrees -> DMS".into()), tags: vec!["geo".into()], attributes: det }))?;
    Ok(())
}
