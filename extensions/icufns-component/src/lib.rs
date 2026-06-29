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
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-collation" });
use duckdb::extension::{collation, runtime, types};
use exports::duckdb::extension::guest;
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
// Per-row scalar logic, UNCHANGED from the major-3 hand-written impl.
fn scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
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
            // Single-argument, locale-bound sort-key scalars. Each is the
            // transform of one collation (icu_en/icu_sv/icu_de). A collation
            // transform is (text) -> sort-key for ONE locale, so the locale is
            // baked into the variant rather than passed as an argument.
            F::SortKeyLocale(loc) => match text_arg(&args, 0) {
                Some(t) => match sort_key_hex(&t, loc) {
                    Some(s) => types::Duckvalue::Text(s.into()),
                    None => types::Duckvalue::Null,
                },
                None => types::Duckvalue::Null,
            },
        })
}
datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = scalar;
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
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("hex UCA sort key; ORDER BY workaround for COLLATE".into()), tags: vec!["text".into()], attributes: det }))?;
    // icu_compare(a, b, locale) -> int
    let h = NEXT.fetch_add(1, AtOrdering::Relaxed); handlers().lock().unwrap().insert(h, F::Compare);
    reg.register("icu_compare", &[
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("locale".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("locale-aware compare -> -1/0/1".into()), tags: vec!["text".into()], attributes: det }))?;
    // icu_casefold(text) -> text
    let h = NEXT.fetch_add(1, AtOrdering::Relaxed); handlers().lock().unwrap().insert(h, F::CaseFold);
    reg.register("icu_casefold", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("full Unicode case folding".into()), tags: vec!["text".into()], attributes: det }))?;
    // Per-locale single-arg sort-key scalars + their collations. For each locale
    // we register icu_sortkey_<loc>(text) -> sort-key text, then declare a
    // collation icu_<loc> whose transform IS that scalar. `ORDER BY x COLLATE
    // icu_sv` then sorts in Swedish locale order ('z' before 'ä').
    for &loc in COLLATION_LOCALES {
        let scalar_name = format!("icu_sortkey_{loc}");
        let h = NEXT.fetch_add(1, AtOrdering::Relaxed);
        handlers().lock().unwrap().insert(h, F::SortKeyLocale(loc));
        reg.register(&scalar_name,
            &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(format!("locale-bound ({loc}) UCA sort key; transform for COLLATE icu_{loc}")),
                tags: vec!["text".into()], attributes: det }))?;
        // Declare the collation reusing the scalar just registered. Non-combinable
        // (a locale collation replaces, not stacks with, another locale's).
        collation::register_collation(&format!("icu_{loc}"), &scalar_name, false)?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum F { SortKey, Compare, CaseFold, SortKeyLocale(&'static str) }
// The locales we expose as first-class collations. Each gets a single-arg
// sort-key scalar (icu_sortkey_<loc>) plus a collation (icu_<loc>) whose
// transform is that scalar.
const COLLATION_LOCALES: &[&str] = &["en", "sv", "de"];
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
