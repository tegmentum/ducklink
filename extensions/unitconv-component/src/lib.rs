//! Physical unit conversion as a DuckDB scalar (hand-rolled):
//!   unit_convert(value, from, to) -> double. Supports length, mass, and
//!   temperature; case-insensitive unit names. Unknown unit or cross-category
//!   conversion -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "unitconv".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
/// Linear units: factor to the category's base (metre for length, kilogram for
/// mass). Temperature is handled separately because it is affine.
fn length_factor(u: &str) -> Option<f64> {
    Some(match u {
        "m" | "metre" | "meter" => 1.0, "km" | "kilometre" | "kilometer" => 1000.0,
        "cm" | "centimetre" | "centimeter" => 0.01, "mm" | "millimetre" | "millimeter" => 0.001,
        "um" | "micron" => 1e-6, "mi" | "mile" => 1609.344, "yd" | "yard" => 0.9144,
        "ft" | "foot" | "feet" => 0.3048, "in" | "inch" => 0.0254, "nmi" => 1852.0,
        _ => return None,
    })
}
fn mass_factor(u: &str) -> Option<f64> {
    Some(match u {
        "kg" | "kilogram" => 1.0, "g" | "gram" => 0.001, "mg" | "milligram" => 1e-6,
        "t" | "tonne" => 1000.0, "lb" | "pound" => 0.453_592_37, "oz" | "ounce" => 0.028_349_523_125,
        "st" | "stone" => 6.350_293_18, _ => return None,
    })
}
fn temp_to_kelvin(v: f64, u: &str) -> Option<f64> {
    Some(match u { "c" | "celsius" => v + 273.15, "f" | "fahrenheit" => (v - 32.0) * 5.0 / 9.0 + 273.15, "k" | "kelvin" => v, _ => return None })
}
fn kelvin_to(k: f64, u: &str) -> Option<f64> {
    Some(match u { "c" | "celsius" => k - 273.15, "f" | "fahrenheit" => (k - 273.15) * 9.0 / 5.0 + 32.0, "k" | "kelvin" => k, _ => return None })
}
fn convert(value: f64, from: &str, to: &str) -> Option<f64> {
    let (from, to) = (from.trim().to_ascii_lowercase(), to.trim().to_ascii_lowercase());
    if let (Some(a), Some(b)) = (length_factor(&from), length_factor(&to)) { return Some(value * a / b); }
    if let (Some(a), Some(b)) = (mass_factor(&from), mass_factor(&to)) { return Some(value * a / b); }
    if let Some(k) = temp_to_kelvin(value, &from) { return kelvin_to(k, &to); }
    None
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
        let value = match args.first() {
            Some(types::Duckvalue::Float64(v)) => *v, Some(types::Duckvalue::Int64(v)) => *v as f64,
            _ => return Ok(types::Duckvalue::Null) };
        let from = match args.get(1) { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let to = match args.get(2) { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        Ok(match convert(value, &from, &to) { Some(r) => types::Duckvalue::Float64(r), None => types::Duckvalue::Null })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("unitconv: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("unitconv: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("unitconv: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("unitconv: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("unit_convert", &[
        runtime::Funcarg { name: Some("value".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("from".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("to".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Float64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("convert between units".into()), tags: vec!["units".into()], attributes: det }))?;
    Ok(())
}
