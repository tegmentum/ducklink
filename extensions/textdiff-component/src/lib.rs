//! Text diffing as DuckDB scalars (via the `similar` crate):
//!   text_diff(a, b)          -> unified (line-based) diff of a -> b, "" if identical,
//!   diff_ratio(a, b)         -> similarity ratio in [0, 1] (TextDiff::ratio),
//!   diff_changed_lines(a, b) -> count of inserted + deleted lines.
//!   NULL on any NULL input. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use similar::{ChangeTag, TextDiff};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "textdiff".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// A standard line-based unified diff of `a` -> `b`. Empty string if identical.
fn unified(a: &str, b: &str) -> std::string::String {
    if a == b { return std::string::String::new(); }
    let diff = TextDiff::from_lines(a, b);
    let mut out = std::string::String::new();
    for group in diff.grouped_ops(3) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(sign);
                out.push_str(change.value());
                if !change.value().ends_with('\n') { out.push('\n'); }
            }
        }
    }
    out
}
// Character-granularity ratio so single-line strings yield a meaningful score
// (line granularity would score two differing one-liners as 0). This mirrors
// difflib's SequenceMatcher.ratio() over characters.
fn ratio(a: &str, b: &str) -> f64 { TextDiff::from_chars(a, b).ratio() as f64 }
fn changed_lines(a: &str, b: &str) -> i64 {
    let diff = TextDiff::from_lines(a, b);
    diff.iter_all_changes()
        .filter(|c| matches!(c.tag(), ChangeTag::Delete | ChangeTag::Insert))
        .count() as i64
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
        let (a, b) = match (text_arg(&args, 0), text_arg(&args, 1)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(types::Duckvalue::Null),
        };
        Ok(match which {
            D::Diff => types::Duckvalue::Text(unified(&a, &b).into()),
            D::Ratio => types::Duckvalue::Float64(ratio(&a, &b)),
            D::Changed => types::Duckvalue::Int64(changed_lines(&a, &b)),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("textdiff: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("textdiff: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("textdiff: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("textdiff: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let args = || vec![
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text },
    ];
    for (name, d, ret, desc) in [
        ("text_diff", D::Diff, types::Logicaltype::Text, "unified line-based diff of a -> b ('' if identical)"),
        ("diff_ratio", D::Ratio, types::Logicaltype::Float64, "character-level similarity ratio of a and b in [0,1]"),
        ("diff_changed_lines", D::Changed, types::Logicaltype::Int64, "count of inserted + deleted lines"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, d);
        reg.register(name, &args(), ret, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["text".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum D { Diff, Ratio, Changed }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, D>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, D>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
