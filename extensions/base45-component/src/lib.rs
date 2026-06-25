//! Base45 codec (RFC 9285) as DuckDB scalar functions for BLOB <-> VARCHAR.
//!
//!   base45_encode(blob) -> VARCHAR   RFC 9285 encoding
//!   base45_decode(text) -> BLOB      NULL on invalid base45 input
//!
//! Base45 is the transport encoding used by the EU Digital COVID Certificate
//! (QR payloads). Decode errors return NULL rather than raising, so SQL
//! composition stays clean (e.g. COALESCE(base45_decode(x), default_blob)).
//! Round-trips are exact: base45_decode(base45_encode(b)) == b. The codec never
//! panics; every fallible path maps to NULL or a typed Duckerror.

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
            name: "base45".into(),
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

// ---- Arg helpers ----

fn arg_blob(args: &[types::Duckvalue], i: usize, fname: &str) -> Result<Vec<u8>, types::Duckerror> {
    match args.get(i) {
        Some(types::Duckvalue::Blob(b)) => Ok(b.clone()),
        Some(types::Duckvalue::Text(s)) => Ok(s.as_bytes().to_vec()),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{fname}: expected BLOB arg at position {i}"
        ))),
    }
}

fn arg_text(args: &[types::Duckvalue], i: usize, fname: &str) -> Result<String, types::Duckerror> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Ok(s.clone()),
        Some(types::Duckvalue::Blob(b)) => String::from_utf8(b.clone()).map_err(|_| {
            types::Duckerror::Invalidargument(format!("{fname}: BLOB is not valid UTF-8"))
        }),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{fname}: expected VARCHAR arg at position {i}"
        ))),
    }
}

// ---- Dispatch ----

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        // Batched dispatch: the host hands the whole chunk in one WIT call; loop
        // the rows here in-wasm (row i's index is ctx.rowindex + i).
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, args) in rows.into_iter().enumerate() {
            let row_ctx = types::Invokeinfo {
                rowindex: Some(base + i as u64),
                iswindow: ctx.iswindow,
            };
            out.push(Self::call_scalar(handle, args, row_ctx)?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        // A NULL input argument yields a NULL result for both functions.
        if matches!(args.first(), Some(types::Duckvalue::Null)) {
            return Ok(types::Duckvalue::Null);
        }

        let which = scalar_handlers()
            .lock()
            .expect("scalar handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;

        Ok(match which {
            ScalarHandler::Encode => {
                let b = arg_blob(&args, 0, "base45_encode")?;
                types::Duckvalue::Text(base45::encode(&b).into())
            }
            ScalarHandler::Decode => {
                let t = arg_text(&args, 0, "base45_decode")?;
                match base45::decode(&t) {
                    Ok(b) => types::Duckvalue::Blob(b),
                    Err(_) => types::Duckvalue::Null,
                }
            }
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "base45: no table functions".into(),
        ))
    }

    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("base45: no aggregates".into()))
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("base45: no pragmas".into()))
    }

    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("base45: no casts".into()))
    }
}

export!(Extension);

// ---- Registration ----

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ))
        }
    };

    register_one(
        &registry,
        "base45_encode",
        types::Logicaltype::Blob,
        types::Logicaltype::Text,
        ScalarHandler::Encode,
    )?;
    register_one(
        &registry,
        "base45_decode",
        types::Logicaltype::Text,
        types::Logicaltype::Blob,
        ScalarHandler::Decode,
    )?;
    Ok(())
}

fn register_one(
    registry: &runtime::ScalarRegistry,
    name: &str,
    arg: types::Logicaltype,
    returns: types::Logicaltype,
    handler: ScalarHandler,
) -> Result<(), types::Duckerror> {
    let handle = NEXT_SCALAR_HANDLE.fetch_add(1, Ordering::Relaxed);
    scalar_handlers()
        .lock()
        .expect("scalar handler mutex poisoned")
        .insert(handle, handler);

    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("value".into()),
        logical: arg,
    }];
    let opts = runtime::Funcopts {
        description: Some("Base45 codec (RFC 9285)".into()),
        tags: vec!["base45".into()],
        attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
    };
    registry.register(name, &args, &returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Encode,
    Decode,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
