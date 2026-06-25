//! Stop-word detection + removal as DuckDB scalars (via `stop-words`):
//!   is_stopword(word, language) -> bool, remove_stopwords(text, language) -> text.
//!   language is a name or ISO 639-1 code (english/en, french/fr, german/de,
//!   spanish/es, italian/it, portuguese/pt, dutch/nl, russian/ru, finnish/fi,
//!   danish/da, swedish/sv; default english). Unknown language -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "stopwords".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// Allowlist of ISO 639-1 codes `stop_words::get` accepts (it PANICS otherwise).
fn lang_code(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "english" | "en" | "" => Some("en"), "french" | "fr" => Some("fr"), "german" | "de" => Some("de"),
        "spanish" | "es" => Some("es"), "italian" | "it" => Some("it"), "portuguese" | "pt" => Some("pt"),
        "dutch" | "nl" => Some("nl"), "russian" | "ru" => Some("ru"), "finnish" | "fi" => Some("fi"),
        "danish" | "da" => Some("da"), "swedish" | "sv" => Some("sv"), _ => None,
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
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let input = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let code = match text_arg(&args, 1).as_deref().map(lang_code).unwrap_or(Some("en")) { Some(c) => c, None => return Ok(types::Duckvalue::Null) };
        let list = stop_words::get(code);
        if handle == 1 {
            let w = input.to_ascii_lowercase();
            Ok(types::Duckvalue::Boolean(list.iter().any(|s| *s == w)))
        } else {
            let kept: std::vec::Vec<&str> = input.split_whitespace()
                .filter(|w| { let lw = w.to_ascii_lowercase(); !list.iter().any(|s| *s == lw) }).collect();
            Ok(types::Duckvalue::Text(kept.join(" ").into()))
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("stopwords: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("stopwords: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("stopwords: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("stopwords: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("is_stopword", &[
        runtime::Funcarg { name: Some("word".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("language".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Boolean, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("is a stop word?".into()), tags: vec!["nlp".into()], attributes: det }))?;
    reg.register("remove_stopwords", &[
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("language".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("strip stop words".into()), tags: vec!["nlp".into()], attributes: det }))?;
    Ok(())
}
