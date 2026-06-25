//! SQL pretty-printer as DuckDB scalars (via the `sqlformat` crate):
//!   sql_format(sql)          -> reindented SQL (2-space indent, keyword case left
//!                               as written — uppercase conversion is OFF).
//!   sql_format_compact(sql)  -> single-line / condensed SQL (collapsed whitespace).
//!   NULL input -> NULL output. Any other input is always formatted; never panics.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use sqlformat::{format, FormatOptions, Indent, QueryParams};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "sqlformat".into(),
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

/// Shared formatting defaults: 2-space indent, keyword-case preserved
/// (`uppercase: None` => no case conversion). Everything else is the crate
/// default (generic dialect, one trailing line break).
fn base_options(inline: bool) -> FormatOptions<'static> {
    FormatOptions {
        indent: Indent::Spaces(2),
        uppercase: None,
        inline,
        ..Default::default()
    }
}

/// Reindented, multi-line form.
fn pretty(sql: &str) -> String {
    format(sql, &QueryParams::None, &base_options(false)).into()
}

/// Single-line condensed form: format inline, then collapse every run of
/// whitespace (including the newlines `sqlformat` may still emit between
/// statements) to a single space.
fn compact(sql: &str) -> String {
    let formatted = format(sql, &QueryParams::None, &base_options(true));
    formatted.split_whitespace().collect::<std::vec::Vec<_>>().join(" ").into()
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<&str> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.as_str()),
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

        // NULL in -> NULL out. A non-text, non-null argument is an error.
        let sql = match args.first() {
            Some(types::Duckvalue::Null) | None => return Ok(types::Duckvalue::Null),
            Some(types::Duckvalue::Text(_)) => text_arg(&args, 0).unwrap(),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlformat expects a single VARCHAR argument".into(),
                ))
            }
        };

        let out = match which {
            F::Pretty => pretty(sql),
            F::Compact => compact(sql),
        };
        Ok(types::Duckvalue::Text(out))
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlformat: no table fns".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlformat: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlformat: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlformat: no casts".into()))
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

    for (name, f, desc) in [
        ("sql_format", F::Pretty, "Reindent SQL (2-space indent, keyword case preserved)"),
        ("sql_format_compact", F::Compact, "Condense SQL onto a single line"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        reg.register(
            name,
            &[runtime::Funcarg { name: Some("sql".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Text,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["text".into(), "sql".into()],
                attributes: det,
            }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum F {
    Pretty,
    Compact,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
