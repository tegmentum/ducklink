//! Text/terminal visualization as DuckDB scalars:
//!   plot_sparkline(nums_json) -> text  (unicode block sparkline U+2581..U+2588),
//!   plot_bars(nums_json, width) -> text (multi-line horizontal bar chart),
//!   qr_utf8(text) -> text (compact UTF-8 QR rendering).
//!   NULL / invalid -> NULL. Never panics.
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
        Ok(types::Loadresult { name: "textplot".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        Some(types::Duckvalue::Float64(v)) => Some(*v as i64),
        _ => None,
    }
}
/// Parse a JSON array of numbers into a Vec<f64>. None on any failure.
fn parse_nums(s: &str) -> Option<std::vec::Vec<f64>> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    if arr.is_empty() { return None; }
    let mut out = std::vec::Vec::with_capacity(arr.len());
    for item in arr {
        let n = item.as_f64()?;
        if !n.is_finite() { return None; }
        out.push(n);
    }
    Some(out)
}
const BLOCKS: [char; 8] = ['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
fn sparkline(nums: &[f64]) -> String {
    let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;
    let mut s = std::string::String::with_capacity(nums.len() * 3);
    for &n in nums {
        let idx = if range <= 0.0 { 0 } else {
            let frac = (n - min) / range;
            ((frac * 7.0).round() as usize).min(7)
        };
        s.push(BLOCKS[idx]);
    }
    s.into()
}
fn bars(nums: &[f64], width: i64) -> Option<String> {
    if width <= 0 { return None; }
    let w = width as usize;
    let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    // Scale relative to the largest magnitude so empty/full bars are meaningful.
    let top = max.max(0.0);
    let bottom = min.min(0.0);
    let span = top - bottom;
    let mut lines = std::vec::Vec::with_capacity(nums.len());
    for &n in nums {
        let frac = if span <= 0.0 { 0.0 } else { (n - bottom) / span };
        let count = (frac * w as f64).round() as usize;
        let count = count.min(w);
        lines.push("#".repeat(count));
    }
    Some(lines.join("\n").into())
}
fn qr(text: &str) -> Option<String> {
    use qrcode::QrCode;
    use qrcode::render::unicode;
    let code = QrCode::new(text.as_bytes()).ok()?;
    let s = code.render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .quiet_zone(false)
        .build();
    Some(s.into())
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
            T::Sparkline => match text_arg(&args, 0).and_then(|s| parse_nums(&s)) {
                Some(nums) => types::Duckvalue::Text(sparkline(&nums)),
                None => types::Duckvalue::Null,
            },
            T::Bars => {
                let nums = text_arg(&args, 0).and_then(|s| parse_nums(&s));
                let width = i64_arg(&args, 1);
                match (nums, width) {
                    (Some(nums), Some(w)) => match bars(&nums, w) {
                        Some(s) => types::Duckvalue::Text(s), None => types::Duckvalue::Null },
                    _ => types::Duckvalue::Null,
                }
            }
            T::Qr => match text_arg(&args, 0).and_then(|s| qr(&s)) {
                Some(s) => types::Duckvalue::Text(s),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("textplot: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("textplot: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("textplot: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("textplot: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, T::Sparkline);
    reg.register("plot_sparkline", &[
        runtime::Funcarg { name: Some("nums_json".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON number array -> unicode sparkline".into()), tags: vec!["viz".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, T::Bars);
    reg.register("plot_bars", &[
        runtime::Funcarg { name: Some("nums_json".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("width".into()), logical: types::Logicaltype::Int64 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON number array -> multi-line bar chart".into()), tags: vec!["viz".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, T::Qr);
    reg.register("qr_utf8", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("text -> compact UTF-8 QR code".into()), tags: vec!["viz".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum T { Sparkline, Bars, Qr }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
