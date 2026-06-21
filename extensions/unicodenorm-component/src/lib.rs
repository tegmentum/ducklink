//! Unicode normalization as DuckDB scalars (via `unicode-normalization`):
//!   nfc / nfd / nfkc / nfkd (text) -> text. Useful for dedup/compare of
//!   visually-identical strings with different code-point sequences. NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use unicode_normalization::UnicodeNormalization;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "unicodenorm".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
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
        let s = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let out: std::string::String = match which {
            N::Nfc => s.nfc().collect(), N::Nfd => s.nfd().collect(),
            N::Nfkc => s.nfkc().collect(), N::Nfkd => s.nfkd().collect(),
        };
        Ok(types::Duckvalue::Text(out.into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("unicodenorm: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("unicodenorm: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("unicodenorm: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("unicodenorm: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "nfc", N::Nfc)?; one(&reg, "nfd", N::Nfd)?; one(&reg, "nfkc", N::Nfkc)?; one(&reg, "nfkd", N::Nfkd)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, n: N) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, n);
    reg.register(name, &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("Unicode normalization".into()), tags: vec!["unicode".into()], attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum N { Nfc, Nfd, Nfkc, Nfkd }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, N>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, N>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
