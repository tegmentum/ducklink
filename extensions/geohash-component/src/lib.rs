//! Geohash encode/decode as DuckDB scalars (via `geohash`):
//!   geohash_encode(lat, lon, precision) -> text,
//!   geohash_decode_lat(hash) -> double, geohash_decode_lon(hash) -> double.
//!   NULL / invalid -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use geohash::Coord;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "geohash".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        _ => None,
    }
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
        Ok(match which {
            G::Encode => {
                let lat = f64_arg(&args, 0); let lon = f64_arg(&args, 1);
                let len = match args.get(2) { Some(types::Duckvalue::Int64(n)) if *n > 0 => *n as usize, _ => 9 };
                match (lat, lon) {
                    (Some(lat), Some(lon)) => match geohash::encode(Coord { x: lon, y: lat }, len) {
                        Ok(s) => types::Duckvalue::Text(s.into()), Err(_) => types::Duckvalue::Null },
                    _ => types::Duckvalue::Null,
                }
            }
            G::Lat | G::Lon => match text_arg(&args, 0).and_then(|s| geohash::decode(&s).ok()) {
                Some((c, _, _)) => types::Duckvalue::Float64(if which == G::Lat { c.y } else { c.x }),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("geohash: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("geohash: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("geohash: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("geohash: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, G::Encode);
    reg.register("geohash_encode", &[
        runtime::Funcarg { name: Some("lat".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("lon".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("precision".into()), logical: types::Logicaltype::Int64 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("lat/lon -> geohash".into()), tags: vec!["geo".into()], attributes: det }))?;
    for (name, g) in [("geohash_decode_lat", G::Lat), ("geohash_decode_lon", G::Lon)] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, g);
        reg.register(name, &[runtime::Funcarg { name: Some("hash".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some("geohash -> coordinate".into()), tags: vec!["geo".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum G { Encode, Lat, Lon }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, G>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, G>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
