//! Credit-card brand detection as a DuckDB scalar (hand-rolled IIN ranges):
//!   card_brand(number) -> text (visa / mastercard / amex / discover / diners /
//!   jcb / unionpay / maestro / unknown). Non-digits stripped. NULL -> NULL.
//!   Complements the `creditcard` (Luhn) extension.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "cardtype".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn prefix(d: &str, n: usize) -> u32 { d.get(..n).and_then(|s| s.parse().ok()).unwrap_or(0) }
fn brand(raw: &str) -> &'static str {
    let d: std::string::String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let len = d.len();
    if !(12..=19).contains(&len) { return "unknown"; }
    let p2 = prefix(&d, 2); let p4 = prefix(&d, 4); let p6 = prefix(&d, 6); let p3 = prefix(&d, 3);
    if d.starts_with('4') { "visa" }
    else if (p2 == 34 || p2 == 37) && len == 15 { "amex" }
    else if (51..=55).contains(&p2) || (2221..=2720).contains(&p4) { "mastercard" }
    else if p4 == 6011 || p2 == 65 || (644..=649).contains(&p3) || (622126..=622925).contains(&p6) { "discover" }
    else if (3528..=3589).contains(&p4) { "jcb" }
    else if (300..=305).contains(&p3) || p3 == 309 || p2 == 36 || p2 == 38 { "diners" }
    else if p2 == 62 { "unionpay" }
    else if p2 == 50 || (56..=69).contains(&p2) { "maestro" }
    else { "unknown" }
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
        match args.first() {
            Some(types::Duckvalue::Text(s)) => Ok(types::Duckvalue::Text(brand(s).into())),
            _ => Ok(types::Duckvalue::Null),
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("cardtype: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("cardtype: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("cardtype: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("cardtype: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("card_brand", &[runtime::Funcarg { name: Some("number".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("credit-card brand".into()), tags: vec!["validation".into()], attributes: det }))?;
    Ok(())
}
