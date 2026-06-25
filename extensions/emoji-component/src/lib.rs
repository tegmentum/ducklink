//! Emoji lookups as DuckDB scalars (via `emojis`):
//!   emoji_name(emoji) -> text, emoji_shortcode(emoji) -> text,
//!   emoji_char(shortcode) -> text (shortcode without colons). Unknown -> NULL.
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
        Ok(types::Loadresult { name: "emoji".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
        let s = match arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            E::Name => match emojis::get(s.trim()) { Some(e) => types::Duckvalue::Text(e.name().into()), None => types::Duckvalue::Null },
            E::Shortcode => match emojis::get(s.trim()).and_then(|e| e.shortcode()) {
                Some(c) => types::Duckvalue::Text(c.into()), None => types::Duckvalue::Null },
            E::Char => match emojis::get_by_shortcode(s.trim().trim_matches(':')) {
                Some(e) => types::Duckvalue::Text(e.as_str().into()), None => types::Duckvalue::Null },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("emoji: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("emoji: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("emoji: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("emoji: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "emoji_name", det, E::Name, "emoji")?;
    one(&reg, "emoji_shortcode", det, E::Shortcode, "emoji")?;
    one(&reg, "emoji_char", det, E::Char, "shortcode")?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, attr: types::Funcflags, e: E, argname: &str) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, e);
    reg.register(name, &[runtime::Funcarg { name: Some(argname.into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("emoji lookup".into()), tags: vec!["emoji".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum E { Name, Shortcode, Char }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, E>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, E>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
