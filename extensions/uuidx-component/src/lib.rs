//! UUID extras as DuckDB scalars (via the `uuid` crate), complementing DuckDB's
//! built-in v4 `uuid()`:
//!
//!   uuid_v7()              -> VARCHAR  generate a new time-ordered UUIDv7 (volatile)
//!   uuid_version(text)     -> BIGINT   the version field (1..8) of a UUID
//!   uuid_timestamp(text)   -> BIGINT   embedded unix-ms timestamp (v7/v1), else NULL
//!
//! Parse failures -> NULL. `uuid_v7` is non-deterministic (clock + random).

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

use uuid::Uuid;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "uuidx".into(),
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

fn arg_text(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
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
        Ok(match which {
            ScalarHandler::V7 => types::Duckvalue::Text(Uuid::now_v7().to_string()),
            ScalarHandler::Version => match arg_text(&args, 0).and_then(|s| Uuid::parse_str(&s).ok()) {
                Some(u) => types::Duckvalue::Int64(u.get_version_num() as i64),
                None => types::Duckvalue::Null,
            },
            ScalarHandler::Timestamp => {
                match arg_text(&args, 0).and_then(|s| Uuid::parse_str(&s).ok()).and_then(|u| u.get_timestamp()) {
                    Some(ts) => {
                        let (secs, nanos) = ts.to_unix();
                        types::Duckvalue::Int64(secs as i64 * 1000 + (nanos / 1_000_000) as i64)
                    }
                    None => types::Duckvalue::Null,
                }
            }
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuidx: no table functions".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuidx: no aggregates".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuidx: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuidx: no casts".into()))
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
    // uuid_v7 is volatile: a fresh value per call (clock + random), so NOT deterministic.
    register(&registry, "uuid_v7", &[], types::Logicaltype::Text, types::Funcflags::empty(), ScalarHandler::V7)?;
    register(&registry, "uuid_version", &["uuid"], types::Logicaltype::Int64, det, ScalarHandler::Version)?;
    register(&registry, "uuid_timestamp", &["uuid"], types::Logicaltype::Int64, det, ScalarHandler::Timestamp)?;
    Ok(())
}

fn register(
    registry: &runtime::ScalarRegistry,
    name: &str,
    arg_names: &[&str],
    returns: types::Logicaltype,
    attributes: types::Funcflags,
    handler: ScalarHandler,
) -> Result<(), types::Duckerror> {
    let handle = NEXT_SCALAR_HANDLE.fetch_add(1, Ordering::Relaxed);
    scalar_handlers().lock().expect("scalar handler mutex poisoned").insert(handle, handler);
    let callback = runtime::ScalarCallback::new(handle);
    let args: std::vec::Vec<runtime::Funcarg> = arg_names
        .iter()
        .map(|n| runtime::Funcarg { name: Some((*n).into()), logical: types::Logicaltype::Text })
        .collect();
    let opts = runtime::Funcopts {
        description: Some("UUID helper".into()),
        tags: vec!["uuid".into()],
        attributes,
    };
    registry.register(name, &args, &returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    V7,
    Version,
    Timestamp,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
