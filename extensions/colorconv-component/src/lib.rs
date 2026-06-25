//! HSL / HSV colour conversions as DuckDB scalars (hand-rolled):
//!   hex_to_hsl(hex) -> "h,s,l", hex_to_hsv(hex) -> "h,s,v" (h 0-360, s/l/v
//!   0-100), hsl_to_hex(h, s, l) -> "#rrggbb". Complements csscolor + color.
//!   Bad hex / NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "colorconv".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn parse_hex(s: &str) -> Option<(f64, f64, f64)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 { return None; }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}
fn hue(r: f64, g: f64, b: f64, max: f64, d: f64) -> f64 {
    if d == 0.0 { return 0.0; }
    let h = if max == r { (g - b) / d + if g < b { 6.0 } else { 0.0 } }
        else if max == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
    h * 60.0
}
fn to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b); let min = r.min(g).min(b); let d = max - min;
    let l = (max + min) / 2.0;
    let s = if d == 0.0 { 0.0 } else { d / (1.0 - (2.0 * l - 1.0).abs()) };
    (hue(r, g, b, max, d), s * 100.0, l * 100.0)
}
fn to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b); let min = r.min(g).min(b); let d = max - min;
    let s = if max == 0.0 { 0.0 } else { d / max };
    (hue(r, g, b, max, d), s * 100.0, max * 100.0)
}
fn hue2rgb(p: f64, q: f64, mut t: f64) -> f64 {
    if t < 0.0 { t += 1.0; } if t > 1.0 { t -= 1.0; }
    if t < 1.0 / 6.0 { p + (q - p) * 6.0 * t }
    else if t < 1.0 / 2.0 { q }
    else if t < 2.0 / 3.0 { p + (q - p) * (2.0 / 3.0 - t) * 6.0 }
    else { p }
}
fn hsl_to_hex(h: f64, s: f64, l: f64) -> std::string::String {
    let (h, s, l) = (h / 360.0, (s / 100.0).clamp(0.0, 1.0), (l / 100.0).clamp(0.0, 1.0));
    let (r, g, b) = if s == 0.0 { (l, l, l) } else {
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        (hue2rgb(p, q, h + 1.0 / 3.0), hue2rgb(p, q, h), hue2rgb(p, q, h - 1.0 / 3.0))
    };
    format!("#{:02x}{:02x}{:02x}", (r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}
fn fmt3(a: f64, b: f64, c: f64) -> std::string::String {
    format!("{},{},{}", a.round() as i64, b.round() as i64, c.round() as i64)
}
fn num(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) { Some(types::Duckvalue::Float64(v)) => Some(*v), Some(types::Duckvalue::Int64(v)) => Some(*v as f64), _ => None }
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
            C::Hsl | C::Hsv => {
                let hex = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
                let (r, g, b) = match parse_hex(&hex) { Some(t) => t, None => return Ok(types::Duckvalue::Null) };
                let (a, bb, c) = if which == C::Hsl { to_hsl(r, g, b) } else { to_hsv(r, g, b) };
                types::Duckvalue::Text(fmt3(a, bb, c).into())
            }
            C::ToHex => match (num(&args, 0), num(&args, 1), num(&args, 2)) {
                (Some(h), Some(s), Some(l)) => types::Duckvalue::Text(hsl_to_hex(h, s, l).into()),
                _ => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("colorconv: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("colorconv: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("colorconv: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("colorconv: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, C::Hsl);
    reg.register("hex_to_hsl", &[runtime::Funcarg { name: Some("hex".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("hex -> HSL".into()), tags: vec!["color".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, C::Hsv);
    reg.register("hex_to_hsv", &[runtime::Funcarg { name: Some("hex".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("hex -> HSV".into()), tags: vec!["color".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, C::ToHex);
    reg.register("hsl_to_hex", &[
        runtime::Funcarg { name: Some("h".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("s".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("l".into()), logical: types::Logicaltype::Float64 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("HSL -> hex".into()), tags: vec!["color".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum C { Hsl, Hsv, ToHex }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, C>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, C>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
