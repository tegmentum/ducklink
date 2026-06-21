//! WCAG color math as DuckDB scalars (parsing via `csscolorparser`):
//!   color_luminance(css) -> double (relative luminance 0..1),
//!   color_contrast(a, b) -> double (contrast ratio 1..21). NULL/unparseable -> NULL.
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
        Ok(types::Loadresult { name: "color".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// WCAG 2.x relative luminance from a CSS color string.
fn luminance(css: &str) -> Option<f64> {
    let c = csscolorparser::parse(css).ok()?;
    let lin = |v: f32| { let v = v as f64; if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } };
    Some(0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b))
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
        let contrast = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        if contrast {
            let (a, b) = match (arg(&args, 0).and_then(|s| luminance(&s)), arg(&args, 1).and_then(|s| luminance(&s))) {
                (Some(a), Some(b)) => (a, b), _ => return Ok(types::Duckvalue::Null) };
            let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
            return Ok(types::Duckvalue::Float64((hi + 0.05) / (lo + 0.05)));
        }
        Ok(match arg(&args, 0).and_then(|s| luminance(&s)) {
            Some(l) => types::Duckvalue::Float64(l), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("color: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("color: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("color: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("color: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h1 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h1, false);
    reg.register("color_luminance", &[runtime::Funcarg { name: Some("css".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Float64, runtime::ScalarCallback::new(h1),
        Some(&runtime::Funcopts { description: Some("WCAG relative luminance".into()), tags: vec!["color".into()], attributes: det }))?;
    let h2 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h2, true);
    reg.register("color_contrast", &[
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Float64, runtime::ScalarCallback::new(h2),
        Some(&runtime::Funcopts { description: Some("WCAG contrast ratio".into()), tags: vec!["color".into()], attributes: det }))?;
    Ok(())
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, bool>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
