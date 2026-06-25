//! WHATWG URLPattern matching as DuckDB scalars (via the `urlpattern` crate):
//!   url_pattern_test(pattern, url)  -> bool : does `url` match the URLPattern `pattern`?
//!   url_pattern_match(pattern, url) -> text : JSON of matched named groups, or NULL.
//!   Invalid pattern / no match / non-text args -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use url::Url;
use urlpattern::{UrlPattern, UrlPatternInit, UrlPatternMatchInput, UrlPatternOptions, UrlPatternComponentResult};

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "urlpattern".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.to_string()), _ => None }
}

/// Build a compiled pattern + parsed input URL. Returns None on any invalid input.
fn build(pattern: &str, url_str: &str) -> Option<(UrlPattern, Url)> {
    let url = Url::parse(url_str).ok()?;
    // `url` doubles as the base so relative patterns (no protocol) resolve.
    let init = UrlPatternInit::parse_constructor_string::<regex::Regex>(pattern, Some(url.clone())).ok()?;
    let p = UrlPattern::parse(init, UrlPatternOptions::default()).ok()?;
    Some((p, url))
}

fn do_test(pattern: &str, url_str: &str) -> Option<bool> {
    let (p, url) = build(pattern, url_str)?;
    p.test(UrlPatternMatchInput::Url(url)).ok()
}

fn do_match(pattern: &str, url_str: &str) -> Option<std::string::String> {
    let (p, url) = build(pattern, url_str)?;
    let res = p.exec(UrlPatternMatchInput::Url(url)).ok()??;
    let mut top = serde_json::Map::new();
    let mut add = |name: &str, c: &UrlPatternComponentResult| {
        let mut m = serde_json::Map::new();
        for (k, v) in &c.groups {
            // Keep only named groups with a non-empty captured value
            // (unnamed wildcard groups get numeric keys / empty values).
            if let Some(v) = v {
                if !v.is_empty() && k.parse::<u64>().is_err() {
                    m.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
            }
        }
        if !m.is_empty() { top.insert(name.to_string(), serde_json::Value::Object(m)); }
    };
    add("protocol", &res.protocol);
    add("hostname", &res.hostname);
    add("port", &res.port);
    add("pathname", &res.pathname);
    add("search", &res.search);
    add("hash", &res.hash);
    Some(serde_json::Value::Object(top).to_string())
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
        let pattern = text_arg(&args, 0);
        let url = text_arg(&args, 1);
        let (pattern, url) = match (pattern, url) { (Some(p), Some(u)) => (p, u), _ => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            F::Test => match do_test(&pattern, &url) {
                Some(b) => types::Duckvalue::Boolean(b),
                None => types::Duckvalue::Null,
            },
            F::Match => match do_match(&pattern, &url) {
                Some(s) => types::Duckvalue::Text(s.into()),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("urlpattern: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("urlpattern: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("urlpattern: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("urlpattern: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let args = [
        runtime::Funcarg { name: Some("pattern".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("url".into()), logical: types::Logicaltype::Text },
    ];
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Test);
    reg.register("url_pattern_test", &args, &types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("does url match URLPattern pattern?".into()), tags: vec!["networking".into()], attributes: det }))?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Match);
    reg.register("url_pattern_match", &args, &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON of matched URLPattern named groups".into()), tags: vec!["networking".into()], attributes: det }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum F { Test, Match }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
