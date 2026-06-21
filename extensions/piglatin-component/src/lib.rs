//! Pig Latin as a DuckDB scalar (hand-rolled): to_pig_latin(text) moves the
//! leading consonant cluster of each word to the end and adds "ay"; vowel-initial
//! words get "way". Case of the word's first letter is preserved. NULL -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "piglatin".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn is_vowel(c: char) -> bool { "aeiouAEIOU".contains(c) }
fn pig_word(w: &str) -> std::string::String {
    let chars: std::vec::Vec<char> = w.chars().collect();
    if chars.is_empty() || !chars[0].is_ascii_alphabetic() { return w.to_string(); }
    if is_vowel(chars[0]) { return format!("{}way", w); }
    let split = chars.iter().position(|c| is_vowel(*c) || !c.is_ascii_alphabetic()).unwrap_or(chars.len());
    let (lead, rest): (std::string::String, std::string::String) = (chars[..split].iter().collect(), chars[split..].iter().collect());
    let cap = chars[0].is_ascii_uppercase();
    let mut moved = format!("{}{}ay", rest, lead.to_lowercase());
    if cap {
        let mut c = moved.chars();
        if let Some(f) = c.next() { moved = format!("{}{}", f.to_ascii_uppercase(), c.as_str()); }
    }
    moved
}
fn to_pig_latin(s: &str) -> std::string::String {
    s.split(' ').map(pig_word).collect::<std::vec::Vec<_>>().join(" ")
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
        match args.first() { Some(types::Duckvalue::Text(s)) => Ok(types::Duckvalue::Text(to_pig_latin(s).into())), _ => Ok(types::Duckvalue::Null) }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("piglatin: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("piglatin: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("piglatin: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("piglatin: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("to_pig_latin", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("Pig Latin".into()), tags: vec!["text".into()], attributes: det }))?;
    Ok(())
}
