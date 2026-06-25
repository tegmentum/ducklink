//! JSON Schema validation as DuckDB scalars (via the `jsonschema` crate):
//!   json_schema_valid(schema, doc)  -> boolean,
//!   json_schema_errors(schema, doc) -> text (JSON array of error messages).
//!   NULL / unparseable input -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "json_schema".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// Parse both args as JSON. Returns None if either is NULL/missing or not valid JSON.
fn parse_pair(args: &[types::Duckvalue]) -> Option<(serde_json::Value, serde_json::Value)> {
    let schema_txt = text_arg(args, 0)?;
    let doc_txt = text_arg(args, 1)?;
    let schema: serde_json::Value = serde_json::from_str(&schema_txt).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&doc_txt).ok()?;
    Some((schema, doc))
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
        Ok(match which {
            J::Valid => match parse_pair(&args) {
                Some((schema, doc)) => match jsonschema::validator_for(&schema) {
                    Ok(v) => types::Duckvalue::Boolean(v.is_valid(&doc)),
                    Err(_) => types::Duckvalue::Null,
                },
                None => types::Duckvalue::Null,
            },
            J::Errors => match parse_pair(&args) {
                Some((schema, doc)) => match jsonschema::validator_for(&schema) {
                    Ok(v) => {
                        let msgs: std::vec::Vec<serde_json::Value> =
                            v.iter_errors(&doc).map(|e| serde_json::Value::String(e.to_string())).collect();
                        let arr = serde_json::Value::Array(msgs);
                        types::Duckvalue::Text(arr.to_string().into())
                    }
                    Err(_) => types::Duckvalue::Null,
                },
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("json_schema: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("json_schema: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("json_schema: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("json_schema: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let valid_h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(valid_h, J::Valid);
    reg.register("json_schema_valid", &[
        runtime::Funcarg { name: Some("schema".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("doc".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Boolean, runtime::ScalarCallback::new(valid_h),
        Some(&runtime::Funcopts { description: Some("true iff doc validates against JSON Schema".into()), tags: vec!["json".into()], attributes: det }))?;
    let errs_h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(errs_h, J::Errors);
    reg.register("json_schema_errors", &[
        runtime::Funcarg { name: Some("schema".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("doc".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(errs_h),
        Some(&runtime::Funcopts { description: Some("JSON array of JSON Schema validation errors ('[]' if valid)".into()), tags: vec!["json".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum J { Valid, Errors }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, J>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, J>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
