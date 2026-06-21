//! Open Location Code (plus codes) as DuckDB scalars (via `open-location-code`):
//!   pluscode_encode(lat, lon, length) -> text, pluscode_valid(code) -> bool,
//!   pluscode_decode_lat(code) / pluscode_decode_lon(code) -> double (area
//!   center). Invalid -> NULL/false.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use geo::Point;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "pluscode".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn f(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) { Some(types::Duckvalue::Float64(v)) => Some(*v), Some(types::Duckvalue::Int64(v)) => Some(*v as f64), _ => None }
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
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        match which {
            P::Encode => {
                let (lat, lon) = match (f(&args, 0), f(&args, 1)) { (Some(a), Some(b)) => (a, b), _ => return Ok(types::Duckvalue::Null) };
                let len = match args.get(2) { Some(types::Duckvalue::Int64(n)) if *n >= 2 && *n <= 15 => *n as usize, _ => 10 };
                Ok(types::Duckvalue::Text(open_location_code::encode(Point::new(lon, lat), len).into()))
            }
            P::Valid => Ok(types::Duckvalue::Boolean(text_arg(&args, 0).map(|s| open_location_code::is_valid(s.trim())).unwrap_or(false))),
            P::Lat | P::Lon => {
                let area = match text_arg(&args, 0).and_then(|s| open_location_code::decode(s.trim()).ok()) { Some(a) => a, None => return Ok(types::Duckvalue::Null) };
                Ok(types::Duckvalue::Float64(if which == P::Lat { area.center.lat() } else { area.center.lng() }))
            }
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("pluscode: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("pluscode: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("pluscode: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("pluscode: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, P::Encode);
    reg.register("pluscode_encode", &[
        runtime::Funcarg { name: Some("lat".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("lon".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("length".into()), logical: types::Logicaltype::Int64 }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("lat/lon -> plus code".into()), tags: vec!["geo".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, P::Valid);
    reg.register("pluscode_valid", &[runtime::Funcarg { name: Some("code".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("plus code valid?".into()), tags: vec!["geo".into()], attributes: det }))?;
    for (name, p) in [("pluscode_decode_lat", P::Lat), ("pluscode_decode_lon", P::Lon)] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, p);
        reg.register(name, &[runtime::Funcarg { name: Some("code".into()), logical: types::Logicaltype::Text }],
            types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some("plus code center".into()), tags: vec!["geo".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum P { Encode, Valid, Lat, Lon }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, P>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, P>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
