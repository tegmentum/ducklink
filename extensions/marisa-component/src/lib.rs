//! Trie-backed string set lookup / prefix search as DuckDB scalars (via `fst`):
//!   fst_contains(terms_json, key) -> boolean : is key a member of the set?
//!   fst_prefix(terms_json, prefix) -> varchar : JSON array of terms starting with prefix (sorted).
//!   fst_count(terms_json) -> bigint           : number of distinct terms.
//! NULL / invalid input -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use fst::{Automaton, IntoStreamer, Set, Streamer};
use fst::automaton::Str;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "marisa".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// Parse a JSON array of strings, sort + dedup (lexicographic order required by fst),
/// and build an in-memory fst Set. Returns None on any bad input.
fn build_set(terms_json: &str) -> Option<(Set<std::vec::Vec<u8>>, std::vec::Vec<std::string::String>)> {
    let parsed: serde_json::Value = serde_json::from_str(terms_json).ok()?;
    let arr = parsed.as_array()?;
    let mut terms: std::vec::Vec<std::string::String> = std::vec::Vec::with_capacity(arr.len());
    for v in arr {
        terms.push(v.as_str()?.to_string());
    }
    terms.sort();
    terms.dedup();
    let set = Set::from_iter(terms.iter()).ok()?;
    Some((set, terms))
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
            G::Contains => {
                let terms = text_arg(&args, 0); let key = text_arg(&args, 1);
                match (terms, key) {
                    (Some(t), Some(k)) => match build_set(&t) {
                        Some((set, _)) => types::Duckvalue::Boolean(set.contains(k.as_str())),
                        None => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            G::Prefix => {
                let terms = text_arg(&args, 0); let prefix = text_arg(&args, 1);
                match (terms, prefix) {
                    (Some(t), Some(p)) => match build_set(&t) {
                        Some((set, _)) => {
                            let mut matched: std::vec::Vec<std::string::String> = std::vec::Vec::new();
                            let mut stream = set.search(Str::new(p.as_str()).starts_with()).into_stream();
                            while let Some(k) = stream.next() {
                                if let Ok(s) = std::str::from_utf8(k) { matched.push(s.to_string()); }
                            }
                            match serde_json::to_string(&matched) {
                                Ok(j) => types::Duckvalue::Text(j.into()),
                                Err(_) => types::Duckvalue::Null,
                            }
                        }
                        None => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            G::Count => {
                match text_arg(&args, 0) {
                    Some(t) => match build_set(&t) {
                        Some((set, _)) => types::Duckvalue::Int64(set.len() as i64),
                        None => types::Duckvalue::Null,
                    },
                    None => types::Duckvalue::Null,
                }
            }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("marisa: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("marisa: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("marisa: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("marisa: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, G::Contains);
    reg.register("fst_contains", &[
        runtime::Funcarg { name: Some("terms_json".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("key".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("is key a member of the JSON-array string set?".into()), tags: vec!["trie".into(), "fst".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, G::Prefix);
    reg.register("fst_prefix", &[
        runtime::Funcarg { name: Some("terms_json".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("prefix".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON array of terms starting with prefix (sorted)".into()), tags: vec!["trie".into(), "fst".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, G::Count);
    reg.register("fst_count", &[
        runtime::Funcarg { name: Some("terms_json".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("number of distinct terms in the set".into()), tags: vec!["trie".into(), "fst".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum G { Contains, Prefix, Count }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, G>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, G>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
