//! ICU-style language-sensitive collation scalars (via `icu_collator`, the
//! pure-Rust ICU4X collator with bundled `compiled_data` CLDR tables):
//!   icu_sort_key(text, locale) -> text  (hex-encoded UCA collation sort key;
//!     `ORDER BY icu_sort_key(name,'de')` is the practical WORKAROUND for a real
//!     `COLLATE de`, since the keys compare bytewise in locale-correct order),
//!   icu_compare(a, b, locale) -> int  (-1/0/1 locale-aware comparison),
//!   icu_casefold(text) -> text  (full Unicode case folding, locale-independent).
//!   NULL / unparseable-locale -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering as AtOrdering}, Mutex, OnceLock};
use std::cmp::Ordering;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
use icu_collator::{options::CollatorOptions, CollatorBorrowed};
use icu_locale_core::Locale;
// ---- pure helpers (unit-tested directly; no WIT involvement) ----
// Build a collator for a BCP-47 locale string ("de", "sv", "en", ...). An empty
// or unparseable locale falls back to the CLDR root order rather than failing.
fn collator_for(locale: &str) -> Option<CollatorBorrowed<'static>> {
    let loc: Locale = Locale::try_from_str(locale.trim()).unwrap_or(Locale::UNKNOWN);
    CollatorBorrowed::try_new(loc.into(), CollatorOptions::default()).ok()
}
// Lowercase-hex encoding of a UCA sort key. Keys produced for the same locale
// compare bytewise in collation order, so they can drive ORDER BY directly.
fn sort_key_hex(text: &str, locale: &str) -> Option<std::string::String> {
    let c = collator_for(locale)?;
    let mut key: std::vec::Vec<u8> = std::vec::Vec::new();
    c.write_sort_key_to(text, &mut key).ok()?;
    let mut out = std::string::String::with_capacity(key.len() * 2);
    for b in key {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    Some(out)
}
// Locale-aware comparison collapsed to -1 / 0 / 1.
fn compare_locale(a: &str, b: &str, locale: &str) -> Option<i64> {
    let c = collator_for(locale)?;
    Some(match c.compare(a, b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}
// Full Unicode case folding (per the Unicode CaseFolding.txt full mapping).
fn casefold(text: &str) -> std::string::String {
    caseless::default_case_fold_str(text)
}
// ---- WIT glue ----
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "icufns".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
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
            F::SortKey => {
                match (text_arg(&args, 0), text_arg(&args, 1)) {
                    (Some(t), Some(loc)) => match sort_key_hex(&t, &loc) {
                        Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
                    _ => types::Duckvalue::Null,
                }
            }
            F::Compare => {
                match (text_arg(&args, 0), text_arg(&args, 1), text_arg(&args, 2)) {
                    (Some(a), Some(b), Some(loc)) => match compare_locale(&a, &b, &loc) {
                        Some(n) => types::Duckvalue::Int64(n), None => types::Duckvalue::Null },
                    _ => types::Duckvalue::Null,
                }
            }
            F::CaseFold => match text_arg(&args, 0) {
                Some(t) => types::Duckvalue::Text(casefold(&t).into()),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("icufns: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("icufns: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("icufns: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("icufns: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    // icu_sort_key(text, locale) -> text
    let h = NEXT.fetch_add(1, AtOrdering::Relaxed); handlers().lock().unwrap().insert(h, F::SortKey);
    reg.register("icu_sort_key", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("locale".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("hex UCA sort key; ORDER BY workaround for COLLATE".into()), tags: vec!["text".into()], attributes: det }))?;
    // icu_compare(a, b, locale) -> int
    let h = NEXT.fetch_add(1, AtOrdering::Relaxed); handlers().lock().unwrap().insert(h, F::Compare);
    reg.register("icu_compare", &[
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("locale".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("locale-aware compare -> -1/0/1".into()), tags: vec!["text".into()], attributes: det }))?;
    // icu_casefold(text) -> text
    let h = NEXT.fetch_add(1, AtOrdering::Relaxed); handlers().lock().unwrap().insert(h, F::CaseFold);
    reg.register("icu_casefold", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("full Unicode case folding".into()), tags: vec!["text".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum F { SortKey, Compare, CaseFold }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compare_basic() {
        // 'a' < 'b' in English
        assert_eq!(compare_locale("a", "b", "en"), Some(-1));
        assert_eq!(compare_locale("b", "a", "en"), Some(1));
        assert_eq!(compare_locale("a", "a", "en"), Some(0));
    }
    #[test]
    fn sort_keys_order_consistent() {
        // Sort keys must compare bytewise in collation order.
        let apple = sort_key_hex("apple", "en").unwrap();
        let banana = sort_key_hex("banana", "en").unwrap();
        assert!(apple < banana, "apple={apple} banana={banana}");
        // Key-order must agree with direct comparison.
        let cmp = compare_locale("apple", "banana", "en").unwrap();
        assert_eq!(cmp, -1);
    }
    #[test]
    fn swedish_z_before_a_ring() {
        // In Swedish, 'z' sorts BEFORE 'ä' (ä is near the end of the alphabet),
        // whereas in English 'ä' folds near 'a' and sorts before 'z'.
        assert_eq!(compare_locale("z", "ä", "sv"), Some(-1));
        // Sort keys must reflect the same locale-specific ordering.
        let kz = sort_key_hex("z", "sv").unwrap();
        let ka = sort_key_hex("ä", "sv").unwrap();
        assert!(kz < ka, "sv: key(z)={kz} should be < key(ä)={ka}");
        // English behaves the opposite way for these two.
        assert_eq!(compare_locale("z", "ä", "en"), Some(1));
    }
    #[test]
    fn casefold_full() {
        assert_eq!(casefold("HELLO"), "hello");
        // German sharp s folds to "ss" (full folding, not simple).
        assert_eq!(casefold("STRASSE"), "strasse");
        assert_eq!(casefold("groß"), "gross");
    }
    #[test]
    fn null_locale_falls_back_not_panics() {
        // Garbage locale falls back to root order rather than failing.
        assert!(compare_locale("a", "b", "!!notalocale!!").is_some());
        assert!(sort_key_hex("x", "").is_some());
    }
}
