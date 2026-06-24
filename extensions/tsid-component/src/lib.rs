//! Time-sorted unique IDs (Snowflake / TSID style) as DuckDB scalars.
//!
//! A 64-bit TSID is laid out as:
//!     id = (ms_since_custom_epoch << 22) | (node << 12) | sequence
//! where `ms_since_custom_epoch` is milliseconds since CUSTOM_EPOCH_MS,
//! `node` is a 10-bit machine id and `sequence` is a 12-bit counter.
//!
//! Scalars (all DETERMINISTIC — no clock / RNG, so they are golden-smoke-able):
//!   tsid_encode(id BIGINT)          -> VARCHAR  Crockford base-32 of the 64-bit id
//!   tsid_decode(s VARCHAR)          -> BIGINT   parse base-32 back to i64 (NULL on invalid)
//!   tsid_timestamp(id BIGINT)       -> BIGINT   (id >> 22) + CUSTOM_EPOCH_MS
//!   tsid_from_timestamp(ms BIGINT)  -> BIGINT   ((ms - CUSTOM_EPOCH_MS) << 22), node=seq=0
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

/// TSID standard custom epoch: 2020-01-01T00:00:00Z in milliseconds.
const CUSTOM_EPOCH_MS: i64 = 1_577_836_800_000;

/// Crockford base-32 alphabet (excludes I, L, O, U).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "tsid".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        _ => None,
    }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

/// Encode a 64-bit id as a 13-char Crockford base-32 string (most significant
/// group first). Always 13 chars to cover the full 64-bit range.
fn encode_crockford(id: i64) -> String {
    let u = id as u64;
    let mut buf = [0u8; 13];
    for i in (0..13).rev() {
        let shift = (12 - i) * 5;
        let idx = ((u >> shift) & 0x1f) as usize;
        buf[i] = CROCKFORD[idx];
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

/// Parse a Crockford base-32 string back to a 64-bit id. Returns None on any
/// invalid character. Case-insensitive; I/L map to 1, O maps to 0 (Crockford
/// decode leniency).
fn decode_crockford(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s.len() > 13 { return None; }
    let mut acc: u64 = 0;
    for ch in s.bytes() {
        let v: u64 = match ch.to_ascii_uppercase() {
            b'0' | b'O' => 0,
            b'1' | b'I' | b'L' => 1,
            c @ b'2'..=b'9' => (c - b'0') as u64,
            c @ b'A'..=b'H' => (c - b'A' + 10) as u64,
            b'J' => 18,
            b'K' => 19,
            b'M' => 20,
            b'N' => 21,
            c @ b'P'..=b'T' => (c - b'P' + 22) as u64,
            c @ b'V'..=b'Z' => (c - b'V' + 27) as u64,
            _ => return None,
        };
        acc = acc.checked_mul(32)?.checked_add(v)?;
    }
    Some(acc as i64)
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
        Ok(match which {
            T::Encode => match i64_arg(&args, 0) {
                Some(id) => types::Duckvalue::Text(encode_crockford(id).into()),
                None => types::Duckvalue::Null,
            },
            T::Decode => match text_arg(&args, 0).and_then(|s| decode_crockford(&s)) {
                Some(id) => types::Duckvalue::Int64(id),
                None => types::Duckvalue::Null,
            },
            T::Timestamp => match i64_arg(&args, 0) {
                Some(id) => types::Duckvalue::Int64(((id as u64 >> 22) as i64) + CUSTOM_EPOCH_MS),
                None => types::Duckvalue::Null,
            },
            T::FromTimestamp => match i64_arg(&args, 0) {
                Some(ms) => {
                    let rel = (ms - CUSTOM_EPOCH_MS) as u64;
                    types::Duckvalue::Int64((rel << 22) as i64)
                }
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("tsid: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("tsid: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("tsid: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("tsid: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    reg1(&reg, "tsid_encode", "id", types::Logicaltype::Int64, types::Logicaltype::Text, det, T::Encode, "TSID 64-bit id -> Crockford base-32 string")?;
    reg1(&reg, "tsid_decode", "s", types::Logicaltype::Text, types::Logicaltype::Int64, det, T::Decode, "Crockford base-32 string -> TSID 64-bit id (NULL if invalid)")?;
    reg1(&reg, "tsid_timestamp", "id", types::Logicaltype::Int64, types::Logicaltype::Int64, det, T::Timestamp, "TSID id -> epoch ms")?;
    reg1(&reg, "tsid_from_timestamp", "ms", types::Logicaltype::Int64, types::Logicaltype::Int64, det, T::FromTimestamp, "epoch ms -> TSID id (node=seq=0)")?;
    Ok(())
}

fn reg1(reg: &runtime::ScalarRegistry, name: &str, arg: &str, arg_ty: types::Logicaltype, ret: types::Logicaltype, attr: types::Funcflags, t: T, desc: &str) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, t);
    reg.register(name,
        &[runtime::Funcarg { name: Some(arg.into()), logical: arg_ty }],
        ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["id".into()], attributes: attr }))?;
    Ok(())
}

#[derive(Clone, Copy)] enum T { Encode, Decode, Timestamp, FromTimestamp }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
