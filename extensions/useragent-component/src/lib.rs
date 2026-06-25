//! User-Agent string parsing as DuckDB scalars (via `woothee`):
//!   ua_browser(ua) -> text          (browser name, e.g. 'Chrome')
//!   ua_browser_version(ua) -> text
//!   ua_os(ua) -> text               (OS name)
//!   ua_category(ua) -> text         (device category: pc/smartphone/crawler/...)
//!   ua_is_bot(ua) -> boolean        (true if the UA is a crawler/bot)
//! NULL input -> NULL; otherwise woothee's best-effort result (UNKNOWN when
//! unparseable). Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use woothee::parser::Parser;
use woothee::woothee::VALUE_UNKNOWN;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "useragent".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<&str> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.as_str()), _ => None }
}

/// Extract one field from a parsed UA. Returns owned strings so we never borrow
/// across the woothee `Option` boundary. On a SQL NULL arg -> None (-> NULL).
fn field(args: &[types::Duckvalue], pick: fn(&str) -> std::string::String) -> Option<std::string::String> {
    let ua = text_arg(args, 0)?;
    Some(pick(ua))
}

fn parse_field(ua: &str, f: for<'a> fn(&'a woothee::parser::WootheeResult<'a>) -> &'a str) -> std::string::String {
    match Parser::new().parse(ua) {
        Some(r) => f(&r).to_string(),
        None => VALUE_UNKNOWN.to_string(),
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            U::Browser => match field(&args, |ua| parse_field(ua, |r| r.name)) {
                Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
            U::BrowserVersion => match field(&args, |ua| parse_field(ua, |r| r.version)) {
                Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
            U::Os => match field(&args, |ua| parse_field(ua, |r| r.os)) {
                Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
            U::Category => match field(&args, |ua| parse_field(ua, |r| r.category)) {
                Some(s) => types::Duckvalue::Text(s.into()), None => types::Duckvalue::Null },
            U::IsBot => match text_arg(&args, 0) {
                Some(ua) => {
                    let is = matches!(Parser::new().parse(ua), Some(r) if r.category == "crawler");
                    types::Duckvalue::Boolean(is)
                }
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("useragent: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("useragent: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("useragent: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("useragent: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let ua_arg = || vec![runtime::Funcarg { name: Some("ua".into()), logical: types::Logicaltype::Text }];
    for (name, u, ret, desc) in [
        ("ua_browser", U::Browser, types::Logicaltype::Text, "User-Agent -> browser name"),
        ("ua_browser_version", U::BrowserVersion, types::Logicaltype::Text, "User-Agent -> browser version"),
        ("ua_os", U::Os, types::Logicaltype::Text, "User-Agent -> OS name"),
        ("ua_category", U::Category, types::Logicaltype::Text, "User-Agent -> device category"),
        ("ua_is_bot", U::IsBot, types::Logicaltype::Boolean, "User-Agent -> true if crawler/bot"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, u);
        reg.register(name, &ua_arg(), &ret, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["useragent".into()], attributes: det }))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum U { Browser, BrowserVersion, Os, Category, IsBot }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, U>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, U>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
