//! Great-circle distance as DuckDB scalars (hand-rolled Haversine):
//!   haversine_km(lat1, lon1, lat2, lon2) -> double,
//!   haversine_mi(lat1, lon1, lat2, lon2) -> double. NULL arg -> NULL.
//!   DuckDB core has no distance fn (spatial's ST_Distance is an extension).
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
const EARTH_KM: f64 = 6371.0088;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "haversine".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn f(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) { Some(types::Duckvalue::Float64(v)) => Some(*v), Some(types::Duckvalue::Int64(v)) => Some(*v as f64), _ => None }
}
fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dlat, dlon) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    EARTH_KM * 2.0 * a.sqrt().asin()
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
        let miles = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        match (f(&args, 0), f(&args, 1), f(&args, 2), f(&args, 3)) {
            (Some(a), Some(b), Some(c), Some(d)) => {
                let km = distance_km(a, b, c, d);
                Ok(types::Duckvalue::Float64(if miles { km * 0.621371192 } else { km }))
            }
            _ => Ok(types::Duckvalue::Null),
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("haversine: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("haversine: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("haversine: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("haversine: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let coords = || vec![
        runtime::Funcarg { name: Some("lat1".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("lon1".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("lat2".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("lon2".into()), logical: types::Logicaltype::Float64 },
    ];
    for (name, miles) in [("haversine_km", false), ("haversine_mi", true)] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, miles);
        reg.register(name, &coords(), types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some("great-circle distance".into()), tags: vec!["geo".into()], attributes: det }))?;
    }
    Ok(())
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, bool>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
