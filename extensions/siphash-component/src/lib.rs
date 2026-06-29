//! Keyed SipHash-1-3 as a DuckDB scalar (via `siphasher`):
//!   siphash(key0, key1, text) -> ubigint. Fast keyed hash for hash-flooding-
//!   resistant bucketing. NULL text -> NULL.
use std::hash::Hasher;
use siphasher::sip::SipHasher13;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::guest;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "siphash".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn key(args: &[types::Duckvalue], i: usize) -> u64 {
    match args.get(i) { Some(types::Duckvalue::Int64(n)) => *n as u64, Some(types::Duckvalue::Uint64(n)) => *n, _ => 0 }
}
// Per-row scalar logic, UNCHANGED. NOTE: siphash returns UBIGINT
// (Duckvalue::Uint64), which the neutral pull-up type set cannot express, so this
// stays a hand-written component bridged to the major-4 columnar dispatch via
// datalink_extcore::columnar_bridge!.
fn scalar(_handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
    let text = match args.get(2) { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
    let mut hasher = SipHasher13::new_with_keys(key(&args, 0), key(&args, 1));
    hasher.write(text.as_bytes());
    Ok(types::Duckvalue::Uint64(hasher.finish()))
}

datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = scalar;
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("siphash", &[
        runtime::Funcarg { name: Some("key0".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("key1".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Uint64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("keyed SipHash-1-3".into()), tags: vec!["hash".into()], attributes: det }))?;
    Ok(())
}
