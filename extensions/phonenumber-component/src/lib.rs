//! Phone-number validation + formatting as DuckDB scalars (via `phonenumber`,
//! a Rust port of Google libphonenumber with embedded metadata):
//!   phone_valid(number, region) -> bool, phone_format(number, region, mode) ->
//!   text (mode: e164 / international / national / rfc3966), phone_country_code
//!   (number, region) -> bigint (the country calling code). `region` is a 2-letter
//!   code (e.g. 'US'); empty when the number has a '+' prefix. Unparseable -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use phonenumber::{Mode, country};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "phonenumber".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn parse(args: &[types::Duckvalue]) -> Option<phonenumber::PhoneNumber> {
    let number = text_arg(args, 0)?;
    let region: Option<country::Id> = match text_arg(args, 1) {
        Some(r) if !r.trim().is_empty() => Some(r.trim().to_ascii_uppercase().parse().ok()?),
        _ => None,
    };
    phonenumber::parse(region, number).ok()
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
        if which == F::Valid {
            return Ok(types::Duckvalue::Boolean(parse(&args).map(|n| phonenumber::is_valid(&n)).unwrap_or(false)));
        }
        let n = match parse(&args) { Some(n) => n, None => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            F::Code => types::Duckvalue::Int64(n.code().value() as i64),
            F::Format => {
                let mode = match text_arg(&args, 2).unwrap_or_default().trim().to_ascii_lowercase().as_str() {
                    "e164" => Mode::E164, "national" => Mode::National, "rfc3966" => Mode::Rfc3966, _ => Mode::International,
                };
                types::Duckvalue::Text(n.format().mode(mode).to_string().into())
            }
            F::Valid => unreachable!(),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("phonenumber: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("phonenumber: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("phonenumber: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("phonenumber: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Valid);
    reg.register("phone_valid", &[
        runtime::Funcarg { name: Some("number".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("region".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("valid phone number?".into()), tags: vec!["phone".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Format);
    reg.register("phone_format", &[
        runtime::Funcarg { name: Some("number".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("region".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("mode".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("format phone number".into()), tags: vec!["phone".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Code);
    reg.register("phone_country_code", &[
        runtime::Funcarg { name: Some("number".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("region".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("country calling code".into()), tags: vec!["phone".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum F { Valid, Format, Code }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
