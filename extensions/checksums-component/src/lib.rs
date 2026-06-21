//! Non-cryptographic checksums as DuckDB scalars over the UTF-8 bytes of the
//! input: crc16(text) (CRC-16/ARC), adler32(text), fnv1a_32(text), fnv1a_64(text).
//! Complements crypto (crc32) and hashfuncs (xxhash/murmur). NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use crc::{Crc, CRC_16_ARC};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
const CRC16: Crc<u16> = Crc::<u16>::new(&CRC_16_ARC);
fn fnv1a_32(b: &[u8]) -> u32 { let mut h = 0x811c9dc5u32; for &x in b { h ^= x as u32; h = h.wrapping_mul(0x0100_0193); } h }
fn fnv1a_64(b: &[u8]) -> u64 { let mut h = 0xcbf2_9ce4_8422_2325u64; for &x in b { h ^= x as u64; h = h.wrapping_mul(0x0000_0100_0000_01b3); } h }
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "checksums".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
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
        let b = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone().into_bytes(), _ => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            C::Crc16 => types::Duckvalue::Int64(CRC16.checksum(&b) as i64),
            C::Adler32 => types::Duckvalue::Int64(adler::adler32_slice(&b) as i64),
            C::Fnv32 => types::Duckvalue::Int64(fnv1a_32(&b) as i64),
            C::Fnv64 => types::Duckvalue::Uint64(fnv1a_64(&b)),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("checksums: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("checksums: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("checksums: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("checksums: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "crc16", types::Logicaltype::Int64, det, C::Crc16)?;
    one(&reg, "adler32", types::Logicaltype::Int64, det, C::Adler32)?;
    one(&reg, "fnv1a_32", types::Logicaltype::Int64, det, C::Fnv32)?;
    one(&reg, "fnv1a_64", types::Logicaltype::Uint64, det, C::Fnv64)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, c: C) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, c);
    reg.register(name, &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("checksum".into()), tags: vec!["hash".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum C { Crc16, Adler32, Fnv32, Fnv64 }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, C>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, C>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
