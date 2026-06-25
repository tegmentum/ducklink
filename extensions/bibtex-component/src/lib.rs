//! BibTeX parsing as DuckDB scalars (via the `biblatex` crate):
//!   bibtex_count(bib)   -> BIGINT  number of entries (NULL on parse error)
//!   bibtex_to_json(bib) -> VARCHAR JSON array [{type,key,fields:{...}}] (NULL on error)
//!   bibtex_keys(bib)    -> VARCHAR JSON array of citation keys (NULL on error)
//! NULL / non-text input or parse failure -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use biblatex::{Bibliography, ChunksExt};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "bibtex".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.to_string()), _ => None }
}

/// Minimal RFC 8259 string escaper (we don't pull in serde_json).
fn json_escape(s: &str, out: &mut std::string::String) {
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
}

fn to_json(bib: &Bibliography) -> std::string::String {
    let mut out = std::string::String::from("[");
    for (i, e) in bib.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str("{\"type\":");
        json_escape(&e.entry_type.to_string(), &mut out);
        out.push_str(",\"key\":");
        json_escape(&e.key, &mut out);
        out.push_str(",\"fields\":{");
        for (j, (name, chunks)) in e.fields.iter().enumerate() {
            if j > 0 { out.push(','); }
            json_escape(name, &mut out);
            out.push(':');
            json_escape(&chunks.format_verbatim(), &mut out);
        }
        out.push_str("}}");
    }
    out.push(']');
    out
}

fn keys_json(bib: &Bibliography) -> std::string::String {
    let mut out = std::string::String::from("[");
    for (i, k) in bib.keys().enumerate() {
        if i > 0 { out.push(','); }
        json_escape(k, &mut out);
    }
    out.push(']');
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
        // NULL / non-text input or parse failure -> NULL.
        let src = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let bib = match Bibliography::parse(&src) { Ok(b) => b, Err(_) => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            B::Count => types::Duckvalue::Int64(bib.len() as i64),
            B::ToJson => types::Duckvalue::Text(to_json(&bib).into()),
            B::Keys => types::Duckvalue::Text(keys_json(&bib).into()),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("bibtex: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bibtex: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("bibtex: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bibtex: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let bib_arg = || vec![runtime::Funcarg { name: Some("bib".into()), logical: types::Logicaltype::Text }];

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, B::Count);
    reg.register("bibtex_count", &bib_arg(), &types::Logicaltype::Int64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("Number of BibTeX entries; NULL on parse error".into()), tags: vec!["bibtex".into()], attributes: det }))?;

    for (name, b, desc) in [
        ("bibtex_to_json", B::ToJson, "BibTeX -> JSON array [{type,key,fields}]; NULL on parse error"),
        ("bibtex_keys", B::Keys, "BibTeX -> JSON array of citation keys; NULL on parse error"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, b);
        reg.register(name, &bib_arg(), &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["bibtex".into()], attributes: det }))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum B { Count, ToJson, Keys }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, B>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, B>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
