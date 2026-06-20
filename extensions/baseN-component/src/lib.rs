//! Compact base-N codecs as DuckDB scalar functions: base32 (RFC 4648, no pad)
//! and base58 (Bitcoin alphabet) for BLOB <-> VARCHAR.
//!
//!   base32_encode(blob) -> VARCHAR   RFC 4648 standard, no padding
//!   base32_decode(text) -> BLOB      NULL on invalid input
//!   base58_encode(blob) -> VARCHAR   Bitcoin alphabet
//!   base58_decode(text) -> BLOB      NULL on invalid input
//!
//! Decode errors return NULL rather than raising, so SQL composition stays
//! clean (e.g. COALESCE(base58_decode(x), default_blob)). DB-agnostic logic
//! shared with ~/git/sqlite-wasm's `baseN`; only the registration ABI differs.
//! Crate feature flags follow tooling/compat-registry.json (base32: no default
//! features; bs58: default-features = false + ["alloc"]).

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

const B32: base32::Alphabet = base32::Alphabet::Rfc4648 { padding: false };

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "baseN".into(),
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
        Some(types::Duckvalue::Blob(b)) => {
            String::from_utf8(b.clone()).map_err(|_| {
                types::Duckerror::Invalidargument(format!("{fname}: BLOB is not valid UTF-8"))
            })
        }
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
        let which = scalar_handlers()
            .lock()
            .expect("scalar handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;

        Ok(match which {
            ScalarHandler::Base32Encode => {
                let b = arg_blob(&args, 0, "base32_encode")?;
                types::Duckvalue::Text(base32::encode(B32, &b).into())
            }
            ScalarHandler::Base32Decode => {
                let t = arg_text(&args, 0, "base32_decode")?;
                match base32::decode(B32, &t) {
                    Some(b) => types::Duckvalue::Blob(b),
                    None => types::Duckvalue::Null,
                }
            }
            ScalarHandler::Base58Encode => {
                let b = arg_blob(&args, 0, "base58_encode")?;
                types::Duckvalue::Text(bs58::encode(&b).into_string().into())
            }
            ScalarHandler::Base58Decode => {
                let t = arg_text(&args, 0, "base58_decode")?;
                match bs58::decode(t.as_bytes()).into_vec() {
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
        Err(types::Duckerror::Unsupported("baseN: no table functions".into()))
    }

    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("baseN: no aggregates".into()))
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("baseN: no pragmas".into()))
    }

    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("baseN: no casts".into()))
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

    register_one(&registry, "base32_encode", types::Logicaltype::Blob, types::Logicaltype::Text, ScalarHandler::Base32Encode)?;
    register_one(&registry, "base32_decode", types::Logicaltype::Text, types::Logicaltype::Blob, ScalarHandler::Base32Decode)?;
    register_one(&registry, "base58_encode", types::Logicaltype::Blob, types::Logicaltype::Text, ScalarHandler::Base58Encode)?;
    register_one(&registry, "base58_decode", types::Logicaltype::Text, types::Logicaltype::Blob, ScalarHandler::Base58Decode)?;
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
        description: Some("base-N codec".into()),
        tags: vec!["baseN".into()],
        attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Base32Encode,
    Base32Decode,
    Base58Encode,
    Base58Decode,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
