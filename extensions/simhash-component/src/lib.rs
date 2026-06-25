//! SimHash locality-sensitive fingerprints as DuckDB scalars (hand-rolled):
//!   simhash(text) -> ubigint (64-bit fingerprint over whitespace tokens),
//!   simhash_distance(a, b) -> bigint (Hamming distance of the two fingerprints,
//!   0..64). Near-duplicate texts have small distances. NULL -> NULL.
use std::hash::{Hash, Hasher};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
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
            match text_arg(&args, 0) { Some(s) => Ok(types::Duckvalue::Uint64(simhash(&s))), None => Ok(types::Duckvalue::Null) }
        } else {
            match (text_arg(&args, 0), text_arg(&args, 1)) {
                (Some(a), Some(b)) => Ok(types::Duckvalue::Int64((simhash(&a) ^ simhash(&b)).count_ones() as i64)),
                _ => Ok(types::Duckvalue::Null),
            }
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("simhash: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("simhash: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("simhash: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("simhash: no casts".into())) }
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
