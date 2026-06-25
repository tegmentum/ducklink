//! Additional phonetic algorithms as DuckDB scalars (via `rphonetic`):
//!   nysiis(text), refined_soundex(text), double_metaphone(text) (primary code).
//!   Complements the `phonetic` extension (soundex/metaphone). NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use rphonetic::{Encoder, Nysiis, DoubleMetaphone, RefinedSoundex};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "phonetic2".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
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
        let s = match arg(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let code = match which {
            Ph::Nysiis => Nysiis::default().encode(&s),
            Ph::Refined => RefinedSoundex::default().encode(&s),
            Ph::Double => DoubleMetaphone::default().encode(&s),
        };
        Ok(types::Duckvalue::Text(code.into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("phonetic2: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("phonetic2: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("phonetic2: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("phonetic2: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "nysiis", Ph::Nysiis)?; one(&reg, "refined_soundex", Ph::Refined)?; one(&reg, "double_metaphone", Ph::Double)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, p: Ph) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, p);
    reg.register(name, &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("phonetic code".into()), tags: vec!["phonetic".into()], attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum Ph { Nysiis, Refined, Double }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, Ph>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, Ph>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
