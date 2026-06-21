//! MAC address validation + normalization as DuckDB scalars (via `macaddr`):
//!   mac_valid(text) -> bool, mac_normalize(text) -> text (canonical
//!   colon-separated uppercase; NULL if invalid). NULL -> NULL (valid->false).
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use macaddr::MacAddr;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "mac".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
        let normalize = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let parsed = arg(&args, 0).and_then(|s| MacAddr::from_str(&s).ok());
        Ok(if normalize {
            match parsed { Some(m) => types::Duckvalue::Text(m.to_string().into()), None => types::Duckvalue::Null }
        } else {
            types::Duckvalue::Boolean(parsed.is_some())
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("mac: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("mac: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("mac: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("mac: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h1 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h1, false);
    reg.register("mac_valid", &[runtime::Funcarg { name: Some("mac".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h1),
        Some(&runtime::Funcopts { description: Some("MAC address valid?".into()), tags: vec!["mac".into()], attributes: det }))?;
    let h2 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h2, true);
    reg.register("mac_normalize", &[runtime::Funcarg { name: Some("mac".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h2),
        Some(&runtime::Funcopts { description: Some("canonical MAC".into()), tags: vec!["mac".into()], attributes: det }))?;
    Ok(())
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, bool>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
