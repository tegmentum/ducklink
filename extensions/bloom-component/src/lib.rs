//! Bloom filter as a DuckDB AGGREGATE + query scalar (not in DuckDB core):
//!   bloom_filter(value) AGGREGATE -> text (hex of an 8192-bit, k=5 filter),
//!   bloom_contains(filter_hex, item) -> bool (probabilistic membership; no
//!   false negatives). NULL inputs skipped.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
const M: usize = 8192;          // bits
const BYTES: usize = M / 8;     // 1024
const K: usize = 5;             // hash functions
fn fnv1a(b: &[u8], basis: u64) -> u64 { let mut h = basis; for &x in b { h ^= x as u64; h = h.wrapping_mul(0x0000_0100_0000_01b3); } h }
fn positions(item: &str) -> [usize; K] {
    let h1 = fnv1a(item.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let h2 = fnv1a(item.as_bytes(), 0x8422_2325_cbf2_9ce4) | 1;
    let mut p = [0usize; K];
    for (i, slot) in p.iter_mut().enumerate() { *slot = (h1.wrapping_add((i as u64).wrapping_mul(h2)) % M as u64) as usize; }
    p
}
const HEX: &[u8] = b"0123456789abcdef";
fn hex_encode(b: &[u8]) -> std::string::String { let mut o = std::string::String::with_capacity(b.len()*2); for &x in b { o.push(HEX[(x>>4) as usize] as char); o.push(HEX[(x&0xf) as usize] as char); } o }
fn hex_decode(s: &str) -> Option<std::vec::Vec<u8>> { let s = s.trim(); if s.len()%2!=0 { return None; } (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2],16).ok()).collect() }
fn text(v: &types::Duckvalue) -> Option<&str> { if let types::Duckvalue::Text(s) = v { Some(s) } else { None } }
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register()?;
        Ok(types::Loadresult { name: "bloom".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != 10 { return Err(types::Duckerror::Internal("unknown scalar handle".into())); }
        let filter = match args.first().and_then(text).and_then(hex_decode) { Some(b) if b.len() == BYTES => b, _ => return Ok(types::Duckvalue::Null) };
        let item = match args.get(1).and_then(text) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let present = positions(item).iter().all(|&p| filter[p / 8] & (1 << (p % 8)) != 0);
        Ok(types::Duckvalue::Boolean(present))
    }
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() { out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?); }
        Ok(out)
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("bloom: no table fns".into())) }
    fn call_aggregate(handle: u32, rows: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != 1 { return Err(types::Duckerror::Internal("unknown aggregate handle".into())); }
        let mut bits = vec![0u8; BYTES];
        for row in &rows {
            if let Some(s) = row.first().and_then(text) {
                for &p in positions(s).iter() { bits[p / 8] |= 1 << (p % 8); }
            }
        }
        Ok(types::Duckvalue::Text(hex_encode(&bits).into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("bloom: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bloom: no casts".into())) }
}
export!(Extension);
fn register() -> Result<(), types::Duckerror> {
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    // aggregate
    let acap = runtime::get_capability(types::Capabilitykind::Aggregate).ok_or_else(|| types::Duckerror::Internal("no aggregate capability".into()))?;
    let areg = match acap { runtime::Capability::Aggregate(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    areg.register("bloom_filter", &[runtime::Funcarg { name: Some("value".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::AggregateCallback::new(1),
        Some(&runtime::Funcopts { description: Some("build bloom filter".into()), tags: vec!["sketch".into()], attributes: det }))?;
    // scalar
    let scap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let sreg = match scap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    sreg.register("bloom_contains", &[
        runtime::Funcarg { name: Some("filter".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("item".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Boolean, runtime::ScalarCallback::new(10),
        Some(&runtime::Funcopts { description: Some("bloom membership".into()), tags: vec!["sketch".into()], attributes: det }))?;
    Ok(())
}
