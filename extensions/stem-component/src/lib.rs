//! Snowball word stemming as a DuckDB scalar (via `rust-stemmers`):
//!   stem(word, language) -> text. language is english/french/german/spanish/
//!   italian/portuguese/russian/dutch/swedish/norwegian/danish/finnish (default
//!   english). NULL -> NULL.
use rust_stemmers::{Algorithm, Stemmer};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "stem".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn algorithm(lang: &str) -> Algorithm {
    match lang.trim().to_ascii_lowercase().as_str() {
        "french" | "fr" => Algorithm::French, "german" | "de" => Algorithm::German,
        "spanish" | "es" => Algorithm::Spanish, "italian" | "it" => Algorithm::Italian,
        "portuguese" | "pt" => Algorithm::Portuguese, "russian" | "ru" => Algorithm::Russian,
        "dutch" | "nl" => Algorithm::Dutch, "swedish" | "sv" => Algorithm::Swedish,
        "norwegian" | "no" => Algorithm::Norwegian, "danish" | "da" => Algorithm::Danish,
        "finnish" | "fi" => Algorithm::Finnish, _ => Algorithm::English,
    }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(_handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let word = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let lang = text_arg(&args, 1).unwrap_or_else(|| "english".into());
        let stemmer = Stemmer::create(algorithm(&lang));
        Ok(types::Duckvalue::Text(stemmer.stem(&word.to_lowercase()).into_owned().into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("stem: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("stem: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("stem: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("stem: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("stem", &[
        runtime::Funcarg { name: Some("word".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("language".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("Snowball stem".into()), tags: vec!["nlp".into()], attributes: det }))?;
    Ok(())
}
