//! Statistical AGGREGATES DuckDB core lacks (first use of the aggregate
//! capability): harmonic_mean(x). The host buffers a group's rows and calls
//! call_aggregate once with all of them. NULL inputs are skipped; empty -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_aggregates()?;
        Ok(types::Loadresult { name: "aggstat".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn as_f64(v: &types::Duckvalue) -> Option<f64> {
    match v { types::Duckvalue::Float64(x) => Some(*x), types::Duckvalue::Int64(x) => Some(*x as f64), types::Duckvalue::Uint64(x) => Some(*x as f64), _ => None }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar(_h: u32, _a: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("aggstat: no scalars".into())) }
    fn call_scalar_batch(_h: u32, _r: Vec<Vec<types::Duckvalue>>, _c: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("aggstat: no scalars".into())) }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("aggstat: no table fns".into())) }
    fn call_aggregate(handle: u32, rows: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        match handle {
            1 => {
                // harmonic mean = n / sum(1/x_i), positive values only
                let mut n = 0u64; let mut recip_sum = 0.0f64;
                for row in &rows {
                    if let Some(x) = row.first().and_then(as_f64) {
                        if x != 0.0 { n += 1; recip_sum += 1.0 / x; }
                    }
                }
                if n == 0 || recip_sum == 0.0 { Ok(types::Duckvalue::Null) }
                else { Ok(types::Duckvalue::Float64(n as f64 / recip_sum)) }
            }
            _ => Err(types::Duckerror::Internal("unknown aggregate handle".into())),
        }
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("aggstat: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("aggstat: no casts".into())) }
}
export!(Extension);
fn register_aggregates() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Aggregate).ok_or_else(|| types::Duckerror::Internal("no aggregate capability".into()))?;
    let reg = match cap { runtime::Capability::Aggregate(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("harmonic_mean", &[runtime::Funcarg { name: Some("x".into()), logical: types::Logicaltype::Float64 }],
        &types::Logicaltype::Float64, runtime::AggregateCallback::new(1),
        Some(&runtime::Funcopts { description: Some("harmonic mean".into()), tags: vec!["stats".into()], attributes: det }))?;
    Ok(())
}
