//! Luhn check-digit generation as DuckDB scalars (hand-rolled):
//!   luhn_check_digit(partial) -> bigint (the digit to append),
//!   luhn_append(partial) -> text (partial + check digit). Complements the
//!   `luhn` extension (validation). Non-digits stripped; empty -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "luhngen".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn digits(s: &str) -> std::vec::Vec<u8> { s.chars().filter_map(|c| c.to_digit(10).map(|d| d as u8)).collect() }
/// Check digit so the partial+digit passes Luhn: the last partial digit is the
/// first doubled position (check digit sits at position 1 from the right).
fn check_digit(ds: &[u8]) -> u8 {
    let mut sum = 0u32; let mut double = true;
    for &d in ds.iter().rev() {
        let mut x = d as u32;
        if double { x *= 2; if x > 9 { x -= 9; } }
        sum += x; double = !double;
    }
    ((10 - sum % 10) % 10) as u8
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
        let raw = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let ds = digits(&raw);
        if ds.is_empty() { return Ok(types::Duckvalue::Null); }
        let cd = check_digit(&ds);
        Ok(if handle == 1 {
            types::Duckvalue::Int64(cd as i64)
        } else {
            let body: std::string::String = ds.iter().map(|d| (b'0' + d) as char).collect();
            types::Duckvalue::Text(format!("{}{}", body, cd).into())
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("luhngen: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("luhngen: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("luhngen: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("luhngen: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("luhn_check_digit", &[runtime::Funcarg { name: Some("partial".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("Luhn check digit".into()), tags: vec!["validation".into()], attributes: det }))?;
    reg.register("luhn_append", &[runtime::Funcarg { name: Some("partial".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("append Luhn check digit".into()), tags: vec!["validation".into()], attributes: det }))?;
    Ok(())
}
