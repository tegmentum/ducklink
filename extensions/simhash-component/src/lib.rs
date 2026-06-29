//! SimHash locality-sensitive fingerprints as DuckDB scalars (hand-rolled):
//!   simhash(text) -> ubigint (64-bit fingerprint over whitespace tokens),
//!   simhash_distance(a, b) -> bigint (Hamming distance of the two fingerprints,
//!   0..64). Near-duplicate texts have small distances. NULL -> NULL.
use std::hash::{Hash, Hasher};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::guest;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "simhash".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
/// Deterministic 64-bit token hash (DefaultHasher uses fixed keys 0,0).
fn token_hash(t: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}
fn simhash(text: &str) -> u64 {
    let mut v = [0i32; 64];
    let mut any = false;
    for token in text.split_whitespace() {
        any = true;
        let h = token_hash(&token.to_lowercase());
        for (i, slot) in v.iter_mut().enumerate() {
            if (h >> i) & 1 == 1 { *slot += 1; } else { *slot -= 1; }
        }
    }
    if !any { return 0; }
    let mut out = 0u64;
    for (i, &slot) in v.iter().enumerate() { if slot > 0 { out |= 1u64 << i; } }
    out
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
// Per-row scalar logic, UNCHANGED. NOTE: simhash returns UBIGINT
// (Duckvalue::Uint64), which the neutral pull-up type set (boolean/int64/float64/
// text/blob) cannot express, so this stays a hand-written component bridged to
// the major-4 columnar dispatch via datalink_extcore::columnar_bridge!.
fn scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
    if handle == 1 {
        match text_arg(&args, 0) { Some(s) => Ok(types::Duckvalue::Uint64(simhash(&s))), None => Ok(types::Duckvalue::Null) }
    } else {
        match (text_arg(&args, 0), text_arg(&args, 1)) {
            (Some(a), Some(b)) => Ok(types::Duckvalue::Int64((simhash(&a) ^ simhash(&b)).count_ones() as i64)),
            _ => Ok(types::Duckvalue::Null),
        }
    }
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
    reg.register("simhash", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Uint64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("SimHash fingerprint".into()), tags: vec!["hash".into()], attributes: det }))?;
    reg.register("simhash_distance", &[
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("SimHash Hamming distance".into()), tags: vec!["hash".into()], attributes: det }))?;
    Ok(())
}
