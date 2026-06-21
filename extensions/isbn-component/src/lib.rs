//! ISBN-10 / ISBN-13 validation + normalization as DuckDB scalars (hand-rolled,
//! no crate): isbn_valid(text) -> bool, isbn_normalize(text) -> text (digits
//! only, uppercase check digit; NULL if invalid). NULL -> NULL (valid->false).
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
        Ok(types::Loadresult { name: "isbn".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// Strip separators, keeping ASCII digits and an upper/lowercase X (only valid
/// as the ISBN-10 check digit); returns the canonical body with X uppercased.
fn clean(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
        .map(|c| c.to_ascii_uppercase()).collect()
}
fn valid_isbn10(s: &str) -> bool {
    if s.len() != 10 { return false; }
    let mut sum: i32 = 0;
    for (i, c) in s.chars().enumerate() {
        let v = if i == 9 && c == 'X' { 10 }
            else if let Some(d) = c.to_digit(10) { d as i32 } else { return false };
        sum += (10 - i as i32) * v;
    }
    sum % 11 == 0
}
fn valid_isbn13(s: &str) -> bool {
    if s.len() != 13 { return false; }
    let mut sum: i32 = 0;
    for (i, c) in s.chars().enumerate() {
        let d = match c.to_digit(10) { Some(d) => d as i32, None => return false };
        sum += if i % 2 == 0 { d } else { 3 * d };
    }
    sum % 10 == 0
}
fn is_valid(body: &str) -> bool {
    match body.len() { 10 => valid_isbn10(body), 13 => valid_isbn13(body), _ => false }
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
        let body = arg(&args, 0).map(|s| clean(&s));
        Ok(match (normalize, body) {
            (false, Some(b)) => types::Duckvalue::Boolean(is_valid(&b)),
            (false, None) => types::Duckvalue::Boolean(false),
            (true, Some(b)) if is_valid(&b) => types::Duckvalue::Text(b.into()),
            (true, _) => types::Duckvalue::Null,
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("isbn: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("isbn: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("isbn: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("isbn: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h1 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h1, false);
    reg.register("isbn_valid", &[runtime::Funcarg { name: Some("isbn".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h1),
        Some(&runtime::Funcopts { description: Some("ISBN-10/13 valid?".into()), tags: vec!["isbn".into()], attributes: det }))?;
    let h2 = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h2, true);
    reg.register("isbn_normalize", &[runtime::Funcarg { name: Some("isbn".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h2),
        Some(&runtime::Funcopts { description: Some("digits-only ISBN".into()), tags: vec!["isbn".into()], attributes: det }))?;
    Ok(())
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, bool>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
