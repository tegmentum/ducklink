//! ISO 4217 currency lookups as DuckDB scalars (via `iso_currency`), keyed by
//! alphabetic code: currency_name, currency_symbol, currency_exponent (minor
//! unit digits), currency_numeric. Unknown code / NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use iso_currency::Currency;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "currency".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
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
        let cur = match arg(&args, 0).and_then(|s| Currency::from_code(&s.to_ascii_uppercase())) {
            Some(c) => c, None => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            C::Name => types::Duckvalue::Text(cur.name().into()),
            C::Symbol => types::Duckvalue::Text(cur.symbol().to_string().into()),
            C::Numeric => types::Duckvalue::Int64(cur.numeric() as i64),
            C::Exponent => match cur.exponent() { Some(e) => types::Duckvalue::Int64(e as i64), None => types::Duckvalue::Null },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("currency: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("currency: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("currency: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("currency: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "currency_name", types::Logicaltype::Text, det, C::Name)?;
    one(&reg, "currency_symbol", types::Logicaltype::Text, det, C::Symbol)?;
    one(&reg, "currency_numeric", types::Logicaltype::Int64, det, C::Numeric)?;
    one(&reg, "currency_exponent", types::Logicaltype::Int64, det, C::Exponent)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, c: C) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, c);
    reg.register(name, &[runtime::Funcarg { name: Some("code".into()), logical: types::Logicaltype::Text }],
        &ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("ISO 4217 currency".into()), tags: vec!["currency".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum C { Name, Symbol, Numeric, Exponent }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, C>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, C>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
