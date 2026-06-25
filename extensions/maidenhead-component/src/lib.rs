//! Maidenhead grid locator (ham radio) as DuckDB scalars (hand-rolled math):
//!   to_maidenhead(lat, lon, precision) -> text  (precision = number of pairs),
//!   maidenhead_lat(grid) -> double, maidenhead_lon(grid) -> double  (square center).
//!   NULL / invalid -> NULL, never panic.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;

// --- Maidenhead math -------------------------------------------------------
// Pairs alternate base-18 (letters), base-10 (digits), base-24 (letters), ...
// Pair 0: A..R, 20deg lon / 10deg lat. Pair 1: 0..9. Pair 2: a..x (24).
// From pair 2 on, even pairs are base-24 letters, odd pairs base-10 digits.
fn pair_base(i: usize) -> u32 {
    match i {
        0 => 18,
        1 => 10,
        _ => if i % 2 == 0 { 24 } else { 10 },
    }
}

/// Encode lat/lon to a grid of `pairs` pairs (2*pairs chars). None on bad input.
fn encode(lat: f64, lon: f64, pairs: usize) -> Option<String> {
    if !lat.is_finite() || !lon.is_finite() { return None; }
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) { return None; }
    if pairs == 0 || pairs > 10 { return None; }
    // Work in [0, 360) lon and [0, 180) lat units. Each level splits the parent
    // cell into `base` divisions, so divide the cell size BEFORE indexing.
    let mut lon_u = lon + 180.0;
    let mut lat_u = lat + 90.0;
    let mut lon_cell = 360.0;
    let mut lat_cell = 180.0;
    let mut out = String::new();
    for i in 0..pairs {
        let base = pair_base(i) as f64;
        lon_cell /= base;
        lat_cell /= base;
        // index within this cell
        let mut xi = (lon_u / lon_cell).floor();
        let mut yi = (lat_u / lat_cell).floor();
        // clamp to valid range (handles lat==90 / lon==180 edges)
        let maxv = base - 1.0;
        if xi < 0.0 { xi = 0.0; } else if xi > maxv { xi = maxv; }
        if yi < 0.0 { yi = 0.0; } else if yi > maxv { yi = maxv; }
        out.push(digit(i, xi as u32, true));
        out.push(digit(i, yi as u32, false));
        // descend into the chosen cell
        lon_u -= xi * lon_cell;
        lat_u -= yi * lat_cell;
    }
    Some(out)
}

/// Character for index `v` at pair `i`. `is_lon` only matters cosmetically (same map).
fn digit(i: usize, v: u32, _is_lon: bool) -> char {
    match pair_base(i) {
        18 => (b'A' + v as u8) as char,       // field: A..R, upper
        10 => (b'0' + v as u8) as char,       // square: 0..9
        _ => (b'a' + v as u8) as char,        // subsquare etc: a..x, lower
    }
}

/// Parse a char at pair `i` into its index, plus validate. None if out of range.
fn char_index(i: usize, c: char, is_lon: bool) -> Option<u32> {
    let base = pair_base(i);
    let _ = is_lon;
    let v: u32 = match base {
        18 => {
            let u = c.to_ascii_uppercase();
            if !u.is_ascii_uppercase() { return None; }
            (u as u8 - b'A') as u32
        }
        10 => {
            if !c.is_ascii_digit() { return None; }
            (c as u8 - b'0') as u32
        }
        _ => {
            let l = c.to_ascii_lowercase();
            if !l.is_ascii_lowercase() { return None; }
            (l as u8 - b'a') as u32
        }
    };
    if v >= base { None } else { Some(v) }
}

/// Decode a grid to (lat_center, lon_center). None on invalid grid.
fn decode(grid: &str) -> Option<(f64, f64)> {
    let chars: Vec<char> = grid.trim().chars().collect();
    if chars.is_empty() || chars.len() % 2 != 0 { return None; }
    let pairs = chars.len() / 2;
    if pairs > 10 { return None; }
    let mut lon = 0.0_f64;
    let mut lat = 0.0_f64;
    let mut lon_cell = 360.0_f64;
    let mut lat_cell = 180.0_f64;
    for i in 0..pairs {
        let base = pair_base(i) as f64;
        lon_cell /= base;
        lat_cell /= base;
        let cx = chars[2 * i];
        let cy = chars[2 * i + 1];
        let xi = char_index(i, cx, true)? as f64;
        let yi = char_index(i, cy, false)? as f64;
        lon += xi * lon_cell;
        lat += yi * lat_cell;
    }
    // center of the final (smallest) cell
    lon += lon_cell / 2.0 - 180.0;
    lat += lat_cell / 2.0 - 90.0;
    Some((lat, lon))
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "maidenhead".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
            M::Encode => {
                let lat = f64_arg(&args, 0); let lon = f64_arg(&args, 1);
                let pairs = match args.get(2) { Some(types::Duckvalue::Int64(n)) if *n > 0 => *n as usize, _ => 3 };
                match (lat, lon) {
                    (Some(lat), Some(lon)) => match encode(lat, lon, pairs) {
                        Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
                    _ => types::Duckvalue::Null,
                }
            }
            M::Lat | M::Lon => match text_arg(&args, 0).and_then(|s| decode(&s)) {
                Some((lat, lon)) => types::Duckvalue::Float64(if which == M::Lat { lat } else { lon }),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("maidenhead: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("maidenhead: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("maidenhead: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("maidenhead: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, M::Encode);
    reg.register("to_maidenhead", &[
        runtime::Funcarg { name: Some("lat".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("lon".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("precision".into()), logical: types::Logicaltype::Int64 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("lat/lon -> Maidenhead grid locator".into()), tags: vec!["geo".into(), "ham".into()], attributes: det }))?;
    for (name, m) in [("maidenhead_lat", M::Lat), ("maidenhead_lon", M::Lon)] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, m);
        reg.register(name, &[runtime::Funcarg { name: Some("grid".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some("Maidenhead grid -> square-center coordinate".into()), tags: vec!["geo".into(), "ham".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum M { Encode, Lat, Lon }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, M>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, M>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn munich() {
        // Munich ~ 48.14, 11.60 -> JN58td at 3 pairs
        assert_eq!(encode(48.14, 11.60, 3).as_deref(), Some("JN58td"));
    }
    #[test]
    fn roundtrip() {
        let (lat, lon) = decode("JN58td").unwrap();
        assert!((lat - 48.14).abs() < 0.1, "lat={lat}");
        assert!((lon - 11.60).abs() < 0.2, "lon={lon}");
    }
    #[test]
    fn bad() {
        assert!(encode(200.0, 0.0, 3).is_none());
        assert!(decode("nope!").is_none());
        assert!(decode("J").is_none());
    }
}
