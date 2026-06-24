//! Compile PRQL to SQL as DuckDB scalars (via `prqlc`):
//!   prql_to_sql(prql) -> text   (compile PRQL to SQL; NULL on compile error),
//!   prql_is_valid(prql) -> boolean.
//!   NULL input -> NULL. Never panics.
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
        Ok(types::Loadresult { name: "prql".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

/// Compile a PRQL string to SQL, targeting the DuckDB dialect. Returns None on
/// any compile error. Catches panics defensively so a compiler bug can't unwind
/// across the WIT boundary.
fn compile(src: &str) -> Option<std::string::String> {
    let opts = prqlc::Options::default()
        .no_signature()
        .with_target(prqlc::Target::Sql(Some(prqlc::sql::Dialect::DuckDb)));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prqlc::compile(src, &opts)
    }));
    match res {
        Ok(Ok(sql)) => Some(sql),
        _ => None,
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
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        // NULL input -> NULL / false respectively.
        let src = match text_arg(&args, 0) {
            Some(s) => s,
            None => return Ok(match which {
                P::ToSql => types::Duckvalue::Null,
                P::IsValid => types::Duckvalue::Null,
            }),
        };
        Ok(match which {
            P::ToSql => match compile(&src) {
                Some(sql) => types::Duckvalue::Text(sql.into()),
                None => types::Duckvalue::Null,
            },
            P::IsValid => types::Duckvalue::Boolean(compile(&src).is_some()),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("prql: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("prql: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("prql: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("prql: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, P::ToSql);
    reg.register("prql_to_sql",
        &[runtime::Funcarg { name: Some("prql".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("compile PRQL to SQL (DuckDB dialect); NULL on error".into()), tags: vec!["prql".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, P::IsValid);
    reg.register("prql_is_valid",
        &[runtime::Funcarg { name: Some("prql".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("true if the PRQL compiles".into()), tags: vec!["prql".into()], attributes: det }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum P { ToSql, IsValid }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, P>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, P>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
