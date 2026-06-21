//! Non-cryptographic hash functions as DuckDB scalars (xxHash + MurmurHash3):
//!
//!   xxh32(value)    -> BIGINT   xxHash32 (seed 0)
//!   xxh64(value)    -> UBIGINT  xxHash64 (seed 0)
//!   xxh3(value)     -> UBIGINT  XXH3 64-bit
//!   murmur3(value)  -> BIGINT   MurmurHash3 x86_32 (seed 0)
//!
//! `value` is VARCHAR (hashed as UTF-8 bytes) or BLOB. NULL in -> NULL out.

use std::collections::HashMap;
use std::io::Cursor;
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

use twox_hash::{XxHash32, XxHash3_64, XxHash64};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "hashfuncs".into(),
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

enum Arg {
    Bytes(std::vec::Vec<u8>),
    Null,
}

fn arg_bytes(args: &[types::Duckvalue], i: usize, fname: &str) -> Result<Arg, types::Duckerror> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Ok(Arg::Bytes(s.as_bytes().to_vec())),
        Some(types::Duckvalue::Blob(b)) => Ok(Arg::Bytes(b.clone())),
        Some(types::Duckvalue::Null) => Ok(Arg::Null),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{fname}: expected VARCHAR or BLOB arg at position {i}"
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
        let b = match arg_bytes(&args, 0, "hashfuncs")? {
            Arg::Bytes(b) => b,
            Arg::Null => return Ok(types::Duckvalue::Null),
        };
        Ok(match which {
            ScalarHandler::Xxh32 => types::Duckvalue::Int64(XxHash32::oneshot(0, &b) as i64),
            ScalarHandler::Xxh64 => types::Duckvalue::Uint64(XxHash64::oneshot(0, &b)),
            ScalarHandler::Xxh3 => types::Duckvalue::Uint64(XxHash3_64::oneshot(&b)),
            ScalarHandler::Murmur3 => {
                let h = murmur3::murmur3_32(&mut Cursor::new(&b), 0).unwrap_or(0);
                types::Duckvalue::Int64(h as i64)
            }
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hashfuncs: no table functions".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hashfuncs: no aggregates".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hashfuncs: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hashfuncs: no casts".into()))
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
    register_one(&registry, "xxh32", types::Logicaltype::Int64, det, ScalarHandler::Xxh32)?;
    register_one(&registry, "xxh64", types::Logicaltype::Uint64, det, ScalarHandler::Xxh64)?;
    register_one(&registry, "xxh3", types::Logicaltype::Uint64, det, ScalarHandler::Xxh3)?;
    register_one(&registry, "murmur3", types::Logicaltype::Int64, det, ScalarHandler::Murmur3)?;
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
    let args = vec![runtime::Funcarg { name: Some("value".into()), logical: types::Logicaltype::Text }];
    let opts = runtime::Funcopts {
        description: Some("non-cryptographic hash".into()),
        tags: vec!["hashfuncs".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Xxh32,
    Xxh64,
    Xxh3,
    Murmur3,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
