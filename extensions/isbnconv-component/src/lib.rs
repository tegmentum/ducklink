//! ISBN-10 <-> ISBN-13 conversion as DuckDB scalars (hand-rolled checksums):
//!   isbn10_to_13(isbn10) -> text, isbn13_to_10(isbn13) -> text (only 978-
//!   prefixed). Non-digits (and a trailing X) handled; invalid length -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "isbnconv".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn clean(s: &str) -> std::string::String {
    s.chars().filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x').map(|c| c.to_ascii_uppercase()).collect()
}
fn isbn13_check(body12: &str) -> Option<char> {
    if body12.len() != 12 { return None; }
    let mut sum = 0i32;
    for (i, c) in body12.chars().enumerate() {
        let d = c.to_digit(10)? as i32;
        sum += if i % 2 == 0 { d } else { 3 * d };
    }
    Some((b'0' + ((10 - sum % 10) % 10) as u8) as char)
}
fn isbn10_check(body9: &str) -> Option<char> {
    if body9.len() != 9 { return None; }
    let mut sum = 0i32;
    for (i, c) in body9.chars().enumerate() {
        sum += (10 - i as i32) * c.to_digit(10)? as i32;
    }
    let cd = (11 - sum % 11) % 11;
    Some(if cd == 10 { 'X' } else { (b'0' + cd as u8) as char })
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
        let raw = match args.first() { Some(types::Duckvalue::Text(s)) => clean(s), _ => return Ok(types::Duckvalue::Null) };
        Ok(if handle == 1 {
            // ISBN-10 -> ISBN-13: 978 + first 9 digits + new check
            if raw.len() != 10 { return Ok(types::Duckvalue::Null); }
            let body = format!("978{}", &raw[..9]);
            match isbn13_check(&body) { Some(cd) => types::Duckvalue::Text(format!("{}{}", body, cd).into()), None => types::Duckvalue::Null }
        } else {
            // ISBN-13 -> ISBN-10: only 978-prefixed
            if raw.len() != 13 || !raw.starts_with("978") { return Ok(types::Duckvalue::Null); }
            let body = &raw[3..12];
            match isbn10_check(body) { Some(cd) => types::Duckvalue::Text(format!("{}{}", body, cd).into()), None => types::Duckvalue::Null }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("isbnconv: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("isbnconv: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("isbnconv: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("isbnconv: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("isbn10_to_13", &[runtime::Funcarg { name: Some("isbn10".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("ISBN-10 -> ISBN-13".into()), tags: vec!["isbn".into()], attributes: det }))?;
    reg.register("isbn13_to_10", &[runtime::Funcarg { name: Some("isbn13".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("ISBN-13 -> ISBN-10".into()), tags: vec!["isbn".into()], attributes: det }))?;
    Ok(())
}
