//! Count-Min sketch as a DuckDB AGGREGATE + scalar (not in DuckDB core):
//!   count_min(value) AGGREGATE -> text (hex of a d=4 x w=512 counter table),
//!   cms_estimate(sketch, item) -> bigint (estimated frequency; never an
//!   under-count). NULL inputs skipped.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
const D: usize = 4;
const W: usize = 512;
const CELLS: usize = D * W;
fn fnv1a(b: &[u8]) -> u64 { let mut h = 0xcbf2_9ce4_8422_2325u64; for &x in b { h ^= x as u64; h = h.wrapping_mul(0x0000_0100_0000_01b3); } h }
fn col(base: u64, row: usize) -> usize {
    let a = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1) | 1;
    let b = (row as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    (base.wrapping_mul(a).wrapping_add(b) >> 32) as usize % W
}
const HEX: &[u8] = b"0123456789abcdef";
fn hex_encode(b: &[u8]) -> std::string::String { let mut o = std::string::String::with_capacity(b.len()*2); for &x in b { o.push(HEX[(x>>4) as usize] as char); o.push(HEX[(x&0xf) as usize] as char); } o }
fn hex_decode(s: &str) -> Option<std::vec::Vec<u8>> { let s = s.trim(); if s.len()%2!=0 { return None; } (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2],16).ok()).collect() }
fn text(v: &types::Duckvalue) -> Option<&str> { if let types::Duckvalue::Text(s) = v { Some(s) } else { None } }
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register()?;
        Ok(types::Loadresult { name: "countmin".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != 10 { return Err(types::Duckerror::Internal("unknown scalar handle".into())); }
        let bytes = match args.first().and_then(text).and_then(hex_decode) { Some(b) if b.len() == CELLS * 4 => b, _ => return Ok(types::Duckvalue::Null) };
        let item = match args.get(1).and_then(text) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let base = fnv1a(item.as_bytes());
        let mut est = u32::MAX;
        for row in 0..D {
            let idx = row * W + col(base, row);
            let c = u32::from_be_bytes([bytes[idx*4], bytes[idx*4+1], bytes[idx*4+2], bytes[idx*4+3]]);
            if c < est { est = c; }
        }
        Ok(types::Duckvalue::Int64(est as i64))
    }
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() { out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?); }
        Ok(out)
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("countmin: no table fns".into())) }
    fn call_aggregate(handle: u32, rows: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != 1 { return Err(types::Duckerror::Internal("unknown aggregate handle".into())); }
        let mut table = vec![0u32; CELLS];
        for row in &rows {
            if let Some(s) = row.first().and_then(text) {
                let base = fnv1a(s.as_bytes());
                for r in 0..D { let idx = r * W + col(base, r); table[idx] = table[idx].saturating_add(1); }
            }
        }
        let mut bytes = std::vec::Vec::with_capacity(CELLS * 4);
        for c in &table { bytes.extend_from_slice(&c.to_be_bytes()); }
        Ok(types::Duckvalue::Text(hex_encode(&bytes).into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("countmin: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("countmin: no casts".into())) }
}
export!(Extension);
fn register() -> Result<(), types::Duckerror> {
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let acap = runtime::get_capability(types::Capabilitykind::Aggregate).ok_or_else(|| types::Duckerror::Internal("no aggregate capability".into()))?;
    let areg = match acap { runtime::Capability::Aggregate(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    areg.register("count_min", &[runtime::Funcarg { name: Some("value".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::AggregateCallback::new(1),
        Some(&runtime::Funcopts { description: Some("build Count-Min sketch".into()), tags: vec!["sketch".into()], attributes: det }))?;
    let scap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let sreg = match scap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    sreg.register("cms_estimate", &[
        runtime::Funcarg { name: Some("sketch".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("item".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(10),
        Some(&runtime::Funcopts { description: Some("estimated frequency".into()), tags: vec!["sketch".into()], attributes: det }))?;
    Ok(())
}
