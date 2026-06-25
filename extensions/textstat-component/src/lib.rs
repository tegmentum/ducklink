//! Readability statistics as DuckDB scalars (hand-rolled):
//!   word_count, sentence_count, syllable_count, flesch_reading_ease (higher =
//!   easier), reading_time_minutes (at 200 wpm). NULL / empty -> NULL where a
//!   ratio is undefined, else 0.
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
        Ok(types::Loadresult { name: "textstat".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn syllables(word: &str) -> usize {
    let w = word.to_ascii_lowercase();
    let letters: std::vec::Vec<char> = w.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() { return 0; }
    let vowel = |c: char| "aeiouy".contains(c);
    let mut count = 0usize; let mut prev = false;
    for &c in &letters { let v = vowel(c); if v && !prev { count += 1; } prev = v; }
    if letters.last() == Some(&'e') && count > 1 { count -= 1; }
    count.max(1)
}
fn counts(s: &str) -> (usize, usize, usize) {
    let words: std::vec::Vec<&str> = s.split_whitespace().collect();
    let wc = words.len();
    let sc = s.chars().filter(|c| *c == '.' || *c == '!' || *c == '?').count().max(if wc > 0 { 1 } else { 0 });
    let syl = words.iter().map(|w| syllables(w)).sum();
    (wc, sc, syl)
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
        let s = match text(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let (wc, sc, syl) = counts(&s);
        Ok(match which {
            T::Words => types::Duckvalue::Int64(wc as i64),
            T::Sentences => types::Duckvalue::Int64(sc as i64),
            T::Syllables => types::Duckvalue::Int64(syl as i64),
            T::ReadingTime => types::Duckvalue::Float64(wc as f64 / 200.0),
            T::Flesch => {
                if wc == 0 || sc == 0 { types::Duckvalue::Null }
                else {
                    let score = 206.835 - 1.015 * (wc as f64 / sc as f64) - 84.6 * (syl as f64 / wc as f64);
                    types::Duckvalue::Float64(score)
                }
            }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("textstat: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("textstat: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("textstat: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("textstat: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "word_count", types::Logicaltype::Int64, det, T::Words)?;
    one(&reg, "sentence_count", types::Logicaltype::Int64, det, T::Sentences)?;
    one(&reg, "syllable_count", types::Logicaltype::Int64, det, T::Syllables)?;
    one(&reg, "flesch_reading_ease", types::Logicaltype::Float64, det, T::Flesch)?;
    one(&reg, "reading_time_minutes", types::Logicaltype::Float64, det, T::ReadingTime)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, t: T) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, t);
    reg.register(name, &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("readability".into()), tags: vec!["nlp".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum T { Words, Sentences, Syllables, Flesch, ReadingTime }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
