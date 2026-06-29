//! jq-style JSON query as DuckDB scalars (via the pure-Rust `jaq` interpreter):
//!   jq(json, filter)       -> applies `filter`, returns output(s) as JSON
//!                             (a JSON array when there are multiple outputs).
//!   jq_first(json, filter) -> just the first output as JSON.
//! NULL on parse/eval error; never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::guest;

use jaq_core::{data, load::{Arena, File, Loader}, Compiler, Ctx, Vars};
use jaq_json::Val;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "jaq".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// Apply a jq `filter` to `json`. Returns the collected outputs (each serialized
/// to a JSON string). Returns `None` on any parse / compile / eval error.
fn run_jq(json: &str, filter: &str) -> Option<std::vec::Vec<std::string::String>> {
    // Parse the input JSON into a jaq value.
    let input: Val = jaq_json::read::parse_single(json.as_bytes()).ok()?;

    let program = File { code: filter, path: () };
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).ok()?;

    let filter = Compiler::default().with_funs(funs).compile(modules).ok()?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut out = std::vec::Vec::new();
    for v in filter.id.run((ctx, input)).map(jaq_core::unwrap_valr) {
        let v = v.ok()?;
        out.push(v.to_string());
    }
    Some(out)
}

// Per-row scalar logic, UNCHANGED from the major-3 hand-written impl.
fn scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
    let which = handlers().lock().unwrap().get(&handle).copied()
        .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
    let (json, filter) = match (text_arg(&args, 0), text_arg(&args, 1)) {
        (Some(j), Some(f)) => (j, f),
        _ => return Ok(types::Duckvalue::Null),
    };
    let outputs = match run_jq(&json, &filter) {
        Some(o) => o,
        None => return Ok(types::Duckvalue::Null),
    };
    let result = match which {
        J::Jq => match outputs.len() {
            0 => return Ok(types::Duckvalue::Null),
            1 => outputs.into_iter().next().unwrap(),
            _ => format!("[{}]", outputs.join(",")),
        },
        J::First => match outputs.into_iter().next() {
            Some(s) => s,
            None => return Ok(types::Duckvalue::Null),
        },
    };
    Ok(types::Duckvalue::Text(result.into()))
}
datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = scalar;
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    // jq evaluation is deterministic but not flagged STATELESS-only: keep it deterministic.
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    for (name, j, desc) in [
        ("jq", J::Jq, "Apply a jq filter to JSON; multiple outputs -> JSON array"),
        ("jq_first", J::First, "Apply a jq filter to JSON; first output only"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, j);
        reg.register(name, &[
            runtime::Funcarg { name: Some("json".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("filter".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["json".into()], attributes: det }))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum J { Jq, First }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, J>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, J>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
