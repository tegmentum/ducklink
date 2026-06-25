//! Google Encoded Polyline encode/decode as DuckDB scalars (via the `polyline` crate):
//!   polyline_encode(coords_json, precision) -> text,
//!   polyline_decode(encoded, precision) -> text.
//! Coordinates are JSON arrays of [lon, lat] pairs (lon first, matching geo-types
//! Coord{x=lon, y=lat}). NULL / invalid -> NULL; never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use geo_types::{Coord, LineString};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "polyline".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn precision_arg(args: &[types::Duckvalue], i: usize) -> u32 {
    match args.get(i) {
        Some(types::Duckvalue::Int64(n)) if *n >= 0 && *n <= 11 => *n as u32,
        _ => 5,
    }
}
// JSON array of [lon, lat] pairs -> LineString (Coord{x=lon, y=lat}).
fn json_to_linestring(s: &str) -> Option<LineString<f64>> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let mut coords = std::vec::Vec::with_capacity(arr.len());
    for pair in arr {
        let p = pair.as_array()?;
        if p.len() != 2 { return None; }
        let lon = p[0].as_f64()?;
        let lat = p[1].as_f64()?;
        coords.push(Coord { x: lon, y: lat });
    }
    Some(LineString::new(coords))
}
// LineString -> JSON array of [lon, lat] pairs, rounded to `precision` decimals.
fn linestring_to_json(ls: &LineString<f64>, precision: u32) -> Option<String> {
    let factor = 10f64.powi(precision as i32);
    let pairs: std::vec::Vec<[f64; 2]> = ls.coords()
        .map(|c| [(c.x * factor).round() / factor, (c.y * factor).round() / factor])
        .collect();
    serde_json::to_string(&pairs).ok().map(|s| s.into())
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
        let precision = precision_arg(&args, 1);
        Ok(match which {
            P::Encode => match text_arg(&args, 0).and_then(|s| json_to_linestring(&s)) {
                Some(ls) => match polyline::encode_coordinates(ls, precision) {
                    Ok(s) => types::Duckvalue::Text(s.into()),
                    Err(_) => types::Duckvalue::Null,
                },
                None => types::Duckvalue::Null,
            },
            P::Decode => match text_arg(&args, 0) {
                Some(enc) => match polyline::decode_polyline(&enc, precision) {
                    Ok(ls) => match linestring_to_json(&ls, precision) {
                        Some(j) => types::Duckvalue::Text(j),
                        None => types::Duckvalue::Null,
                    },
                    Err(_) => types::Duckvalue::Null,
                },
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("polyline: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("polyline: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("polyline: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("polyline: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, P::Encode);
    reg.register("polyline_encode", &[
        runtime::Funcarg { name: Some("coords_json".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("precision".into()), logical: types::Logicaltype::Int64 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON [[lon,lat],...] -> encoded polyline".into()), tags: vec!["geo".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, P::Decode);
    reg.register("polyline_decode", &[
        runtime::Funcarg { name: Some("encoded".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("precision".into()), logical: types::Logicaltype::Int64 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("encoded polyline -> JSON [[lon,lat],...]".into()), tags: vec!["geo".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum P { Encode, Decode }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, P>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, P>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
