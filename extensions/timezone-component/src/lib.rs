//! IANA timezone lookups as DuckDB scalars (via `chrono-tz`):
//!   tz_valid(name) -> bool, tz_offset_seconds(name, unix_time) -> bigint
//!   (offset from UTC at that instant, DST-aware), tz_abbreviation(name,
//!   unix_time) -> text (e.g. EST/EDT). Unknown zone / NULL -> NULL (valid->false).
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use chrono::{Offset, TimeZone, Utc};
use chrono_tz::Tz;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "timezone".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) { Some(types::Duckvalue::Int64(n)) => Some(*n), _ => None }
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
        let tz: Option<Tz> = text_arg(&args, 0).and_then(|s| s.trim().parse().ok());
        if which == T::Valid { return Ok(types::Duckvalue::Boolean(tz.is_some())); }
        let tz = match tz { Some(t) => t, None => return Ok(types::Duckvalue::Null) };
        let ts = match i64_arg(&args, 1).and_then(|t| Utc.timestamp_opt(t, 0).single()) { Some(d) => d, None => return Ok(types::Duckvalue::Null) };
        let local = ts.with_timezone(&tz);
        Ok(match which {
            T::Offset => types::Duckvalue::Int64(local.offset().fix().local_minus_utc() as i64),
            T::Abbr => types::Duckvalue::Text(format!("{}", local.offset()).into()),
            T::Valid => unreachable!(),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("timezone: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("timezone: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("timezone: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("timezone: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, T::Valid);
    reg.register("tz_valid", &[runtime::Funcarg { name: Some("name".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("IANA zone valid?".into()), tags: vec!["time".into()], attributes: det }))?;
    for (name, t) in [("tz_offset_seconds", T::Offset), ("tz_abbreviation", T::Abbr)] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, t);
        reg.register(name, &[
            runtime::Funcarg { name: Some("name".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("unix_time".into()), logical: types::Logicaltype::Int64 }],
            if t == T::Offset { &types::Logicaltype::Int64 } else { &types::Logicaltype::Text },
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some("zone offset/abbrev".into()), tags: vec!["time".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum T { Valid, Offset, Abbr }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
