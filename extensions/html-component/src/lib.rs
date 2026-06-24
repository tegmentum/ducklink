//! HTML parsing + CSS-selector extraction as DuckDB scalars (via `scraper`):
//!   html_extract(html, css) -> text of FIRST matching element,
//!   html_extract_all(html, css) -> JSON array of texts of ALL matches,
//!   html_attr(html, css, attr) -> attribute value of the first match.
//! Distinct from `html2text` (which strips a whole document); here extraction is
//! driven by a CSS selector. NULL on NULL input / invalid selector / no match.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use scraper::{Html, Selector};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "html".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

/// Collapse an element's descendant text nodes into a single trimmed string.
fn element_text(el: scraper::ElementRef) -> String {
    let joined: std::string::String = el.text().collect::<std::string::String>();
    joined.split_whitespace().collect::<std::vec::Vec<_>>().join(" ").into()
}

/// Minimal JSON string escaping for the `html_extract_all` array output.
fn json_escape(s: &str) -> std::string::String {
    let mut out = std::string::String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
        // Common args: html (0), css (1). All return NULL on any missing/invalid input.
        let (html, css) = match (text_arg(&args, 0), text_arg(&args, 1)) {
            (Some(h), Some(c)) => (h, c),
            _ => return Ok(types::Duckvalue::Null),
        };
        let sel = match Selector::parse(&css) { Ok(s) => s, Err(_) => return Ok(types::Duckvalue::Null) };
        let doc = Html::parse_fragment(&html);
        Ok(match which {
            H::Extract => match doc.select(&sel).next() {
                Some(el) => types::Duckvalue::Text(element_text(el)),
                None => types::Duckvalue::Null,
            },
            H::ExtractAll => {
                let mut json = std::string::String::from("[");
                let mut first = true;
                for el in doc.select(&sel) {
                    if !first { json.push(','); }
                    first = false;
                    json.push_str(&json_escape(&element_text(el)));
                }
                json.push(']');
                types::Duckvalue::Text(json.into())
            }
            H::Attr => {
                let attr = match text_arg(&args, 2) { Some(a) => a, None => return Ok(types::Duckvalue::Null) };
                match doc.select(&sel).next().and_then(|el| el.value().attr(&attr)) {
                    Some(v) => types::Duckvalue::Text(v.into()),
                    None => types::Duckvalue::Null,
                }
            }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("html: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("html: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("html: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("html: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let html_arg = || runtime::Funcarg { name: Some("html".into()), logical: types::Logicaltype::Text };
    let css_arg = || runtime::Funcarg { name: Some("css".into()), logical: types::Logicaltype::Text };

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, H::Extract);
    reg.register("html_extract", &[html_arg(), css_arg()], types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("text of first element matching CSS selector".into()), tags: vec!["html".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, H::ExtractAll);
    reg.register("html_extract_all", &[html_arg(), css_arg()], types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON array of texts of all matching elements".into()), tags: vec!["html".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, H::Attr);
    reg.register("html_attr", &[html_arg(), css_arg(), runtime::Funcarg { name: Some("attr".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("attribute value of first matching element".into()), tags: vec!["html".into()], attributes: det }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum H { Extract, ExtractAll, Attr }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
