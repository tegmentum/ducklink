//! OpenAI BPE tokenization as DuckDB scalars (via `tiktoken-rs`, embedded BPE
//! tables — no network):
//!   tiktoken_count(text, encoding) -> bigint (token count),
//!   tiktoken_encode(text, encoding) -> text (JSON array of token ids),
//!   tiktoken_decode(ids, encoding) -> text (ids = any integer list, e.g.
//!   "[15339, 1917]"). `encoding` is one of o200k_base / cl100k_base /
//!   p50k_base / p50k_edit / r50k_base (aliases without _base accepted; empty
//!   or unknown -> cl100k_base). NULL text/ids or unusable encoding -> NULL.
//!
//! This mirrors the standalone tiktoken:tokenizer component (~/git/tiktoken-wasm)
//! but in the duckdb:extension WIT world so it joins the embeddable catalog.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use tiktoken_rs::CoreBPE;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "tiktoken".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// Map an encoding name to a freshly-built BPE (tiktoken-rs caches internally).
/// Defaults to cl100k_base when the name is empty or unrecognised.
fn bpe_for(name: Option<String>) -> Option<CoreBPE> {
    let key = name.unwrap_or_default().trim().to_ascii_lowercase();
    match key.as_str() {
        "o200k_base" | "o200k" => tiktoken_rs::o200k_base().ok(),
        "p50k_base" | "p50k" => tiktoken_rs::p50k_base().ok(),
        "p50k_edit" => tiktoken_rs::p50k_edit().ok(),
        "r50k_base" | "r50k" | "gpt2" => tiktoken_rs::r50k_base().ok(),
        _ => tiktoken_rs::cl100k_base().ok(),
    }
}
/// Pull every unsigned integer out of free-form text ("[1, 2, 3]" or "1 2 3").
fn parse_ids(s: &str) -> std::vec::Vec<u32> {
    let mut ids = std::vec::Vec::new();
    let mut cur = std::string::String::new();
    for c in s.chars() {
        if c.is_ascii_digit() { cur.push(c); }
        else if !cur.is_empty() { if let Ok(n) = cur.parse::<u32>() { ids.push(n); } cur.clear(); }
    }
    if let Ok(n) = cur.parse::<u32>() { ids.push(n); }
    ids
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
        let which = handle;
        let input = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let bpe = match bpe_for(text_arg(&args, 1)) { Some(b) => b, None => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            1 => types::Duckvalue::Int64(bpe.encode_ordinary(&input).len() as i64),
            2 => {
                let ids = bpe.encode_ordinary(&input);
                let joined = ids.iter().map(|t| t.to_string()).collect::<std::vec::Vec<_>>().join(",");
                types::Duckvalue::Text(format!("[{}]", joined).into())
            }
            3 => match bpe.decode(parse_ids(&input)) { Ok(s) => types::Duckvalue::Text(s.into()), Err(_) => types::Duckvalue::Null },
            _ => return Err(types::Duckerror::Internal("unknown scalar handle".into())),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("tiktoken: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("tiktoken: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("tiktoken: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("tiktoken: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let enc = || vec![
        runtime::Funcarg { name: Some("input".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("encoding".into()), logical: types::Logicaltype::Text },
    ];
    reg.register("tiktoken_count", &enc(), types::Logicaltype::Int64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("BPE token count".into()), tags: vec!["nlp".into(), "llm".into()], attributes: det }))?;
    reg.register("tiktoken_encode", &enc(), types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("text -> BPE token ids (JSON)".into()), tags: vec!["nlp".into(), "llm".into()], attributes: det }))?;
    reg.register("tiktoken_decode", &enc(), types::Logicaltype::Text, runtime::ScalarCallback::new(3),
        Some(&runtime::Funcopts { description: Some("BPE token ids -> text".into()), tags: vec!["nlp".into(), "llm".into()], attributes: det }))?;
    Ok(())
}
