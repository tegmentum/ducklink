//! BBCode -> HTML as a DuckDB scalar (hand-rolled): bbcode_to_html(text) maps the
//! common paired tags [b][i][u][s][code][quote] and [url=href]text[/url] /
//! [img]src[/img] to HTML. Unknown tags pass through. NULL -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "bbcode".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
/// Replace [url=HREF]TEXT[/url] occurrences left-to-right (regex-free).
fn replace_url(mut s: std::string::String) -> std::string::String {
    loop {
        let Some(start) = s.find("[url=") else { break };
        let Some(rb_rel) = s[start..].find(']') else { break };
        let href_end = start + rb_rel;
        let Some(close_rel) = s[href_end..].find("[/url]") else { break };
        let text_start = href_end + 1;
        let text_end = href_end + close_rel;
        let href = s[start + 5..href_end].to_string();
        let text = s[text_start..text_end].to_string();
        let after = s[text_end + 6..].to_string();
        let before = s[..start].to_string();
        s = format!("{}<a href=\"{}\">{}</a>{}", before, href, text, after);
    }
    s
}
fn bbcode(input: &str) -> std::string::String {
    let mut s = input.to_string();
    for (bb, html) in [
        ("[b]", "<strong>"), ("[/b]", "</strong>"), ("[i]", "<em>"), ("[/i]", "</em>"),
        ("[u]", "<u>"), ("[/u]", "</u>"), ("[s]", "<s>"), ("[/s]", "</s>"),
        ("[code]", "<code>"), ("[/code]", "</code>"), ("[quote]", "<blockquote>"), ("[/quote]", "</blockquote>"),
        ("[img]", "<img src=\""), ("[/img]", "\">"),
    ] {
        s = s.replace(bb, html);
    }
    replace_url(s)
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
        match args.first() { Some(types::Duckvalue::Text(s)) => Ok(types::Duckvalue::Text(bbcode(s).into())), _ => Ok(types::Duckvalue::Null) }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("bbcode: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bbcode: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("bbcode: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("bbcode: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("bbcode_to_html", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("BBCode -> HTML".into()), tags: vec!["markup".into()], attributes: det }))?;
    Ok(())
}
