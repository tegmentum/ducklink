//! Roman numeral encode/decode as DuckDB scalars (via `roman`):
//!   to_roman(int) -> text (NULL outside 1..=3999),
//!   from_roman(text) -> bigint (NULL if not a valid numeral).
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
        Ok(types::Loadresult { name: "roman".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn int_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(n)) => Some(*n),
        Some(types::Duckvalue::Uint64(n)) => i64::try_from(*n).ok(),
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
            H::ToRoman => match int_arg(&args, 0).and_then(|n| i32::try_from(n).ok()).and_then(roman::to) {
                Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
            H::FromRoman => match text_arg(&args, 0).and_then(|s| roman::from(&s)) {
                Some(n) => types::Duckvalue::Int64(n as i64), None => types::Duckvalue::Null },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("roman: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("roman: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("roman: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("roman: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h1 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h1, H::ToRoman);
    reg.register("to_roman",
        &[runtime::Funcarg { name: Some("n".into()), logical: types::Logicaltype::Int64 }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h1),
        Some(&runtime::Funcopts { description: Some("integer -> Roman numeral".into()), tags: vec!["roman".into()], attributes: det }))?;
    let h2 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h2, H::FromRoman);
    reg.register("from_roman",
        &[runtime::Funcarg { name: Some("s".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(h2),
        Some(&runtime::Funcopts { description: Some("Roman numeral -> integer".into()), tags: vec!["roman".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum H { ToRoman, FromRoman }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
