//! Cron expression evaluation as DuckDB scalars (via `croner`), deterministic
//! (UTC, reference time is always an argument — no clock reads):
//!   cron_is_valid(expr) -> boolean,
//!   cron_next(expr, after_unix_ms) -> bigint  (next fire time strictly after, UTC ms),
//!   cron_prev(expr, before_unix_ms) -> bigint (previous fire time strictly before, UTC ms).
//! NULL / invalid expr -> NULL (never panics).
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use chrono::{DateTime, TimeZone, Utc};
use croner::Cron;
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "cron".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        _ => None,
    }
}

fn parse_cron(expr: &str) -> Option<Cron> {
    Cron::from_str(expr).ok()
}

fn ms_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    match Utc.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => Some(dt),
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
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
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
        Ok(match which {
            C::IsValid => match text_arg(&args, 0) {
                Some(expr) => types::Duckvalue::Boolean(parse_cron(&expr).is_some()),
                // NULL expr -> NULL (SQL-style propagation)
                None => types::Duckvalue::Null,
            },
            C::Next | C::Prev => {
                let expr = text_arg(&args, 0);
                let ms = i64_arg(&args, 1);
                match (expr, ms) {
                    (Some(expr), Some(ms)) => {
                        match (parse_cron(&expr), ms_to_dt(ms)) {
                            (Some(cron), Some(dt)) => {
                                // inclusive = false -> strictly after / strictly before
                                let res = if which == C::Next {
                                    cron.find_next_occurrence(&dt, false)
                                } else {
                                    cron.find_previous_occurrence(&dt, false)
                                };
                                match res {
                                    Ok(fire) => types::Duckvalue::Int64(fire.timestamp_millis()),
                                    Err(_) => types::Duckvalue::Null,
                                }
                            }
                            _ => types::Duckvalue::Null,
                        }
                    }
                    _ => types::Duckvalue::Null,
                }
            }
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("cron: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("cron: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("cron: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("cron: no casts".into()))
    }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    // cron_is_valid(expr) -> boolean
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, C::IsValid);
    reg.register(
        "cron_is_valid",
        &[runtime::Funcarg {
            name: Some("expr".into()),
            logical: types::Logicaltype::Text,
        }],
        &types::Logicaltype::Boolean,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("true if expr is a valid cron expression".into()),
            tags: vec!["cron".into()],
            attributes: det,
        }),
    )?;

    // cron_next(expr, after_unix_ms) -> bigint, cron_prev(expr, before_unix_ms) -> bigint
    for (name, c, desc) in [
        (
            "cron_next",
            C::Next,
            "next fire time strictly after after_unix_ms (UTC ms)",
        ),
        (
            "cron_prev",
            C::Prev,
            "previous fire time strictly before before_unix_ms (UTC ms)",
        ),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, c);
        reg.register(
            name,
            &[
                runtime::Funcarg {
                    name: Some("expr".into()),
                    logical: types::Logicaltype::Text,
                },
                runtime::Funcarg {
                    name: Some("ref_unix_ms".into()),
                    logical: types::Logicaltype::Int64,
                },
            ],
            &types::Logicaltype::Int64,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["cron".into()],
                attributes: det,
            }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum C {
    IsValid,
    Next,
    Prev,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, C>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, C>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
