//! HTML + URL percent encode/decode as DuckDB scalars:
//!   html_escape / html_unescape (html-escape),
//!   url_encode / url_decode (percent-encoding). NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use percent_encoding::{utf8_percent_encode, percent_decode_str, NON_ALPHANUMERIC};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "escape".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
        let out = match which {
            H::HtmlEscape => html_escape::encode_text(&s).into_owned(),
            H::HtmlUnescape => html_escape::decode_html_entities(&s).into_owned(),
            H::UrlEncode => utf8_percent_encode(&s, NON_ALPHANUMERIC).to_string(),
            H::UrlDecode => percent_decode_str(&s).decode_utf8_lossy().into_owned(),
        };
        Ok(types::Duckvalue::Text(out.into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("escape: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("escape: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("escape: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("escape: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "html_escape", det, H::HtmlEscape)?;
    one(&reg, "html_unescape", det, H::HtmlUnescape)?;
    one(&reg, "url_encode", det, H::UrlEncode)?;
    one(&reg, "url_decode", det, H::UrlDecode)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(handle, h);
    let cb = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }];
    let opts = runtime::Funcopts { description: Some("escape".into()), tags: vec!["escape".into()], attributes: attr };
    reg.register(name, &args, &types::Logicaltype::Text, cb, Some(&opts))?; Ok(())
}
#[derive(Clone, Copy)] enum H { HtmlEscape, HtmlUnescape, UrlEncode, UrlDecode }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
