//! Bech32 encoding as DuckDB scalars (via `bech32`, BIP-173 checksum):
//!   bech32_encode(hrp, hex) -> text, bech32_hrp(text) -> human-readable part,
//!   bech32_decode_hex(text) -> data bytes as hex, bech32_valid(text) -> bool.
//!   Invalid input -> NULL (valid -> false).
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use bech32::{Bech32, Hrp};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "bech32".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
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
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        match which {
            B::Encode => {
                let (hrp, data) = match (text_arg(&args, 0).and_then(|s| Hrp::parse(s.trim()).ok()), text_arg(&args, 1).and_then(|s| hex_decode(&s))) {
                    (Some(h), Some(d)) => (h, d), _ => return Ok(types::Duckvalue::Null) };
                Ok(match bech32::encode::<Bech32>(hrp, &data) { Ok(s) => types::Duckvalue::Text(s.into()), Err(_) => types::Duckvalue::Null })
            }
            B::Valid => Ok(types::Duckvalue::Boolean(text_arg(&args, 0).map(|s| bech32::decode(s.trim()).is_ok()).unwrap_or(false))),
            B::Hrp | B::DecodeHex => {
                let (hrp, data) = match text_arg(&args, 0).and_then(|s| bech32::decode(s.trim()).ok()) { Some(t) => t, None => return Ok(types::Duckvalue::Null) };
                Ok(types::Duckvalue::Text(if which == B::Hrp { hrp.to_string().into() } else { hex_encode(&data).into() }))
            }
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("bech32: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bech32: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("bech32: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bech32: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, B::Encode);
    reg.register("bech32_encode", &[
        runtime::Funcarg { name: Some("hrp".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("hex".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("bech32 encode".into()), tags: vec!["encoding".into()], attributes: det }))?;
    for (name, b, ret) in [("bech32_hrp", B::Hrp, types::Logicaltype::Text), ("bech32_decode_hex", B::DecodeHex, types::Logicaltype::Text), ("bech32_valid", B::Valid, types::Logicaltype::Boolean)] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, b);
        reg.register(name, &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
            ret, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some("bech32".into()), tags: vec!["encoding".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum B { Encode, Hrp, DecodeHex, Valid }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, B>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, B>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
