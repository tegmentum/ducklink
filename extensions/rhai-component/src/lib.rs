//! Evaluate a Rhai expression as a DuckDB scalar (via the `rhai` crate):
//!   rhai_eval(expr)        -> text   (result rendered to text; NULL on error)
//!   rhai_eval_int(expr)    -> bigint (result coerced to i64; NULL if not int / on error)
//!   rhai_eval_double(expr) -> double (result coerced to f64; NULL on error)
//!
//! The engine is sandboxed: max_operations and max_string_size bound the work a
//! hostile expression can do, and every error path returns NULL rather than panicking.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use rhai::{Dynamic, Engine};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "rhai".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

/// Build a sandboxed engine: bound operations and string size so a hostile
/// expression cannot hang or balloon memory.
fn sandboxed_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(1_000_000);
    engine.set_max_string_size(64 * 1024);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(10_000);
    engine.set_max_expr_depths(64, 64);
    engine
}

/// Evaluate an expression, returning the raw Dynamic result. Errors (parse,
/// runtime, limit) collapse to None.
fn eval(expr: &str) -> Option<Dynamic> {
    sandboxed_engine().eval::<Dynamic>(expr).ok()
}

/// Render a Dynamic result to its text form (int/float/bool/string/etc.).
fn render(value: &Dynamic) -> String {
    value.to_string().into()
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        h: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(
                h,
                a,
                types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow },
            )?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;

        // A NULL or non-text argument yields NULL.
        let expr = match text_arg(&args, 0) {
            Some(s) => s,
            None => return Ok(types::Duckvalue::Null),
        };

        let result = eval(&expr);
        Ok(match which {
            R::Eval => match result {
                Some(v) => types::Duckvalue::Text(render(&v)),
                None => types::Duckvalue::Null,
            },
            R::Int => match result.and_then(|v| v.as_int().ok()) {
                Some(i) => types::Duckvalue::Int64(i),
                None => types::Duckvalue::Null,
            },
            R::Double => match result.and_then(coerce_f64) {
                Some(f) => types::Duckvalue::Float64(f),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rhai: no table fns".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rhai: no aggs".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rhai: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rhai: no casts".into()))
    }
}
export!(Extension);

/// Coerce a Dynamic result to f64, accepting both ints and floats.
fn coerce_f64(value: Dynamic) -> Option<f64> {
    if let Ok(f) = value.as_float() {
        return Some(f);
    }
    if let Ok(i) = value.as_int() {
        return Some(i as f64);
    }
    None
}

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    // Rhai eval is NOT deterministic in general (state, rng if enabled), so we
    // only mark stateless — leave determinism off to be safe.
    let flags = types::Funcflags::empty();

    for (name, which, ret, desc) in [
        ("rhai_eval", R::Eval, types::Logicaltype::Text, "Evaluate a Rhai expression, result as text"),
        ("rhai_eval_int", R::Int, types::Logicaltype::Int64, "Evaluate a Rhai expression, coerce to BIGINT"),
        ("rhai_eval_double", R::Double, types::Logicaltype::Float64, "Evaluate a Rhai expression, coerce to DOUBLE"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, which);
        reg.register(
            name,
            &[runtime::Funcarg { name: Some("expr".into()), logical: types::Logicaltype::Text }],
            ret,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["rhai".into(), "script".into()],
                attributes: flags,
            }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum R {
    Eval,
    Int,
    Double,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, R>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, R>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
