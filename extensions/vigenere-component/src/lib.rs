//! Vigenere cipher as DuckDB scalars (hand-rolled):
//!   vigenere_encrypt(text, key) -> text, vigenere_decrypt(text, key) -> text.
//!   Only letters are enciphered (case preserved); the key cycles over letters
//!   only. NULL / empty key -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "vigenere".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn cipher(text: &str, key: &str, decrypt: bool) -> Option<std::string::String> {
    let shifts: std::vec::Vec<u8> = key.chars().filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase() as u8 - b'a').collect();
    if shifts.is_empty() { return None; }
    let mut ki = 0;
    Some(text.chars().map(|c| {
        let base = if c.is_ascii_uppercase() { b'A' } else if c.is_ascii_lowercase() { b'a' } else { return c };
        let k = shifts[ki % shifts.len()]; ki += 1;
        let k = if decrypt { 26 - k } else { k };
        (((c as u8 - base + k) % 26) + base) as char
    }).collect())
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
        let (text, key) = match (arg(&args, 0), arg(&args, 1)) { (Some(t), Some(k)) => (t, k), _ => return Ok(types::Duckvalue::Null) };
        Ok(match cipher(&text, &key, handle == 2) { Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("vigenere: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("vigenere: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("vigenere: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("vigenere: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("vigenere_encrypt", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("key".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("Vigenere encrypt".into()), tags: vec!["cipher".into()], attributes: det }))?;
    reg.register("vigenere_decrypt", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("key".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("Vigenere decrypt".into()), tags: vec!["cipher".into()], attributes: det }))?;
    Ok(())
}
