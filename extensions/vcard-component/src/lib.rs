//! vCard (.vcf) parsing as DuckDB scalars (via the `ical` crate's
//! VcardParser over the input bytes):
//!   vcard_count(vcf)   -> bigint  -- number of VCARD contacts
//!   vcard_to_json(vcf) -> json    -- [{fn,email,tel,org}, ...]
//!   vcard_names(vcf)   -> json    -- ["formatted name", ...]
//! Parse error / non-text input -> NULL. Never panics.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use ical::VcardParser;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "vcard".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

/// Parse every VCARD in `vcf`, returning all contacts.
/// Any parse error -> None (whole call yields NULL).
fn parse_contacts(vcf: &str) -> Option<std::vec::Vec<ical::parser::vcard::component::VcardContact>> {
    let mut contacts = std::vec::Vec::new();
    for c in VcardParser::new(vcf.as_bytes()) {
        contacts.push(c.ok()?);
    }
    Some(contacts)
}

/// First value of a named property on a contact, if present.
fn prop<'a>(c: &'a ical::parser::vcard::component::VcardContact, name: &str) -> Option<&'a str> {
    c.properties.iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .and_then(|p| p.value.as_deref())
}

fn contacts_to_json(contacts: &[ical::parser::vcard::component::VcardContact]) -> Option<std::string::String> {
    // Output key -> vCard property name.
    let fields = [("fn", "FN"), ("email", "EMAIL"), ("tel", "TEL"), ("org", "ORG")];
    let arr: std::vec::Vec<serde_json::Value> = contacts.iter().map(|c| {
        let mut obj = serde_json::Map::new();
        for (out_key, vcard_name) in fields {
            if let Some(v) = prop(c, vcard_name) {
                obj.insert(out_key.to_string(), serde_json::Value::String(v.to_string()));
            }
        }
        serde_json::Value::Object(obj)
    }).collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
}

fn names_to_json(contacts: &[ical::parser::vcard::component::VcardContact]) -> Option<std::string::String> {
    let arr: std::vec::Vec<serde_json::Value> = contacts.iter()
        .filter_map(|c| prop(c, "FN").map(|s| serde_json::Value::String(s.to_string())))
        .collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
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
        let s = match text(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let contacts = match parse_contacts(&s) { Some(c) => c, None => return Ok(types::Duckvalue::Null) };
        Ok(match handle {
            1 => types::Duckvalue::Int64(contacts.len() as i64),
            2 => match contacts_to_json(&contacts) { Some(j) => types::Duckvalue::Text(j.into()), None => types::Duckvalue::Null },
            3 => match names_to_json(&contacts) { Some(j) => types::Duckvalue::Text(j.into()), None => types::Duckvalue::Null },
            _ => types::Duckvalue::Null,
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("vcard: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("vcard: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("vcard: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("vcard: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let arg = |name: &str| runtime::Funcarg { name: Some(name.into()), logical: types::Logicaltype::Text };
    reg.register("vcard_count", &[arg("vcf")],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("number of VCARD contacts in a vCard string".into()), tags: vec!["vcard".into()], attributes: det }))?;
    reg.register("vcard_to_json", &[arg("vcf")],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("vCard contacts -> JSON array of {fn,email,tel,org}".into()), tags: vec!["vcard".into()], attributes: det }))?;
    reg.register("vcard_names", &[arg("vcf")],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(3),
        Some(&runtime::Funcopts { description: Some("vCard FN (formatted name) values -> JSON array".into()), tags: vec!["vcard".into()], attributes: det }))?;
    Ok(())
}
