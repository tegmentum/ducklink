//! Currency amount formatting as a DuckDB scalar (via `iso_currency`):
//!   format_money(amount, currency_code) -> text, e.g. "$1,234.50", "¥1,000".
//!   Uses the currency's symbol and minor-unit count; thousands-grouped.
//!   Unknown currency / NULL -> NULL.
use iso_currency::Currency;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "money".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
/// Group the integer part of a number string with commas (every 3 digits).
fn group(int_part: &str) -> std::string::String {
    let bytes = int_part.as_bytes();
    let mut out = std::string::String::new();
    let n = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (n - i) % 3 == 0 { out.push(','); }
        out.push(b as char);
    }
    out
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
        let amount = match args.first() {
            Some(types::Duckvalue::Float64(v)) => *v, Some(types::Duckvalue::Int64(v)) => *v as f64,
            _ => return Ok(types::Duckvalue::Null) };
        let cur = match args.get(1) {
            Some(types::Duckvalue::Text(s)) => match Currency::from_code(&s.trim().to_ascii_uppercase()) { Some(c) => c, None => return Ok(types::Duckvalue::Null) },
            _ => return Ok(types::Duckvalue::Null) };
        let decimals = cur.exponent().unwrap_or(2) as usize;
        let neg = amount.is_sign_negative();
        let formatted = format!("{:.*}", decimals, amount.abs());
        let (int_part, frac_part) = match formatted.split_once('.') { Some((i, f)) => (i, Some(f)), None => (formatted.as_str(), None) };
        let mut s = format!("{}{}", cur.symbol(), group(int_part));
        if let Some(f) = frac_part { s.push('.'); s.push_str(f); }
        if neg { s.insert(0, '-'); }
        Ok(types::Duckvalue::Text(s.into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("money: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("money: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("money: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("money: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("format_money", &[
        runtime::Funcarg { name: Some("amount".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("currency".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("format currency amount".into()), tags: vec!["money".into()], attributes: det }))?;
    Ok(())
}
