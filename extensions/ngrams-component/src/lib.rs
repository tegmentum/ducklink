//! Character / word n-grams as DuckDB scalars (output as a JSON array):
//!   char_ngrams(text, n) -> json, word_ngrams(text, n) -> json. n < 1 or
//!   longer than the input yields "[]". NULL -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ngrams".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
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
        let text = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let n = match args.get(1) { Some(types::Duckvalue::Int64(n)) if *n >= 1 => *n as usize, _ => return Ok(types::Duckvalue::Null) };
        let grams: std::vec::Vec<std::string::String> = if handle == 1 {
            let chars: std::vec::Vec<char> = text.chars().collect();
            if chars.len() < n { std::vec::Vec::new() } else { chars.windows(n).map(|w| w.iter().collect()).collect() }
        } else {
            let words: std::vec::Vec<&str> = text.split_whitespace().collect();
            if words.len() < n { std::vec::Vec::new() } else { words.windows(n).map(|w| w.join(" ")).collect() }
        };
        Ok(types::Duckvalue::Text(serde_json::to_string(&grams).unwrap_or_else(|_| "[]".into()).into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ngrams: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ngrams: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ngrams: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ngrams: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    for (name, cb) in [("char_ngrams", 1u32), ("word_ngrams", 2u32)] {
        reg.register(name, &[
            runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("n".into()), logical: types::Logicaltype::Int64 }],
            &types::Logicaltype::Text, runtime::ScalarCallback::new(cb),
            Some(&runtime::Funcopts { description: Some("n-grams (JSON)".into()), tags: vec!["nlp".into()], attributes: det }))?;
    }
    Ok(())
}
