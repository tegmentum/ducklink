//! Fuzzy string matching as DuckDB scalars (via `rapidfuzz`), complementing
//! DuckDB's native levenshtein/jaro_winkler:
//!
//!   fuzz_ratio(a, b)          -> DOUBLE  fuzzywuzzy-style similarity 0..100
//!   damerau_levenshtein(a, b) -> BIGINT  edit distance with transpositions
//!   indel(a, b)               -> BIGINT  insert/delete-only edit distance
//!   osa(a, b)                 -> BIGINT  optimal string alignment distance
//!
//! Two VARCHAR args. NULL in either -> NULL out.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "rapidfuzz".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_keys: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

/// Read a VARCHAR arg; None for SQL NULL.
fn opt_text(args: &[types::Duckvalue], i: usize, fname: &str) -> Result<Option<String>, types::Duckerror> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Ok(Some(s.clone())),
        Some(types::Duckvalue::Null) => Ok(None),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{fname}: expected VARCHAR arg at position {i}"
        ))),
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, args) in rows.into_iter().enumerate() {
            let row_ctx = types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow };
            out.push(Self::call_scalar(handle, args, row_ctx)?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = scalar_handlers()
            .lock()
            .expect("scalar handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let a = opt_text(&args, 0, "rapidfuzz")?;
        let b = opt_text(&args, 1, "rapidfuzz")?;
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(types::Duckvalue::Null),
        };
        Ok(match which {
            ScalarHandler::Ratio => {
                types::Duckvalue::Float64(rapidfuzz::fuzz::ratio(a.chars(), b.chars()) * 100.0)
            }
            ScalarHandler::Damerau => types::Duckvalue::Int64(
                rapidfuzz::distance::damerau_levenshtein::distance(a.chars(), b.chars()) as i64,
            ),
            ScalarHandler::Indel => types::Duckvalue::Int64(
                rapidfuzz::distance::indel::distance(a.chars(), b.chars()) as i64,
            ),
            ScalarHandler::Osa => types::Duckvalue::Int64(
                rapidfuzz::distance::osa::distance(a.chars(), b.chars()) as i64,
            ),
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rapidfuzz: no table functions".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rapidfuzz: no aggregates".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rapidfuzz: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rapidfuzz: no casts".into()))
    }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => return Err(types::Duckerror::Internal("scalar capability returned unexpected variant".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    register_one(&registry, "fuzz_ratio", types::Logicaltype::Float64, det, ScalarHandler::Ratio)?;
    register_one(&registry, "damerau_levenshtein", types::Logicaltype::Int64, det, ScalarHandler::Damerau)?;
    register_one(&registry, "indel", types::Logicaltype::Int64, det, ScalarHandler::Indel)?;
    register_one(&registry, "osa", types::Logicaltype::Int64, det, ScalarHandler::Osa)?;
    Ok(())
}

fn register_one(
    registry: &runtime::ScalarRegistry,
    name: &str,
    returns: types::Logicaltype,
    attributes: types::Funcflags,
    handler: ScalarHandler,
) -> Result<(), types::Duckerror> {
    let handle = NEXT_SCALAR_HANDLE.fetch_add(1, Ordering::Relaxed);
    scalar_handlers().lock().expect("scalar handler mutex poisoned").insert(handle, handler);
    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text },
    ];
    let opts = runtime::Funcopts {
        description: Some("fuzzy string matching".into()),
        tags: vec!["rapidfuzz".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Ratio,
    Damerau,
    Indel,
    Osa,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
