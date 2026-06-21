//! RFC 6238 TOTP codes as a DuckDB scalar:
//!   totp(secret_base32, unix_time, period, digits) -> text (zero-padded).
//!   Deterministic given an explicit time. HMAC-SHA1 over the time counter.
//!   NULL / bad secret -> NULL.
use hmac::{Hmac, Mac};
use sha1::Sha1;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "totp".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn i64_arg(args: &[types::Duckvalue], i: usize, default: i64) -> i64 {
    match args.get(i) { Some(types::Duckvalue::Int64(n)) => *n, _ => default }
}
fn compute(secret: &str, time: i64, period: i64, digits: u32) -> Option<std::string::String> {
    if period <= 0 || !(1..=9).contains(&digits) { return None; }
    let key = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &secret.to_ascii_uppercase())?;
    if key.is_empty() { return None; }
    let counter = (time / period) as u64;
    let mut mac = <Hmac<Sha1>>::new_from_slice(&key).ok()?;
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let bin = ((hash[offset] as u32 & 0x7f) << 24)
        | ((hash[offset + 1] as u32) << 16)
        | ((hash[offset + 2] as u32) << 8)
        | (hash[offset + 3] as u32);
    let code = bin % 10u32.pow(digits);
    Some(format!("{:0width$}", code, width = digits as usize))
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(_handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let secret = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let time = i64_arg(&args, 1, 0);
        let period = i64_arg(&args, 2, 30);
        let digits = i64_arg(&args, 3, 6).clamp(1, 9) as u32;
        Ok(match compute(&secret, time, period, digits) {
            Some(c) => types::Duckvalue::Text(c.into()), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("totp: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("totp: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("totp: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("totp: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("totp", &[
        runtime::Funcarg { name: Some("secret".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("unix_time".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("period".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("digits".into()), logical: types::Logicaltype::Int64 }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("RFC 6238 TOTP".into()), tags: vec!["crypto".into(), "auth".into()], attributes: det }))?;
    Ok(())
}
