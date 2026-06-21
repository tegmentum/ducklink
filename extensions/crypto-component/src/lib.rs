//! Cryptographic + checksum hash digests as DuckDB scalar functions, beyond
//! DuckDB's built-in md5/sha256:
//!
//!   sha1(value)     -> VARCHAR  hex SHA-1 digest
//!   sha512(value)   -> VARCHAR  hex SHA-512 digest
//!   sha3_256(value) -> VARCHAR  hex SHA3-256 digest
//!   blake3(value)   -> VARCHAR  hex BLAKE3 digest
//!   crc32(value)    -> BIGINT   CRC-32 (IEEE) checksum
//!
//! `value` is VARCHAR (hashed as UTF-8 bytes) or BLOB (hashed as raw bytes).
//! NULL in -> NULL out. A component (duckdb:extension world): load it at runtime
//! or embed it -- version-independent, no DuckDB-ABI lock.

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

use sha1::Sha1;
use sha2::Sha512;
use sha3::Sha3_256;
use sha2::Digest; // the Digest trait (shared by sha1/sha2/sha3 via digest crate)

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "crypto".into(),
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

// ---- Hashing (DB-agnostic) ----

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn sha1_hex(b: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(b);
    to_hex(&h.finalize())
}
fn sha512_hex(b: &[u8]) -> String {
    let mut h = Sha512::new();
    h.update(b);
    to_hex(&h.finalize())
}
fn sha3_256_hex(b: &[u8]) -> String {
    let mut h = Sha3_256::new();
    h.update(b);
    to_hex(&h.finalize())
}
fn blake3_hex(b: &[u8]) -> String {
    blake3::hash(b).to_hex().to_string()
}
fn crc32_of(b: &[u8]) -> u32 {
    crc32fast::hash(b)
}

// ---- Arg helper: VARCHAR or BLOB -> bytes; NULL -> None ----

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

// ---- Dispatch ----

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
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
        let b = match arg_bytes(&args, 0, "crypto")? {
            Arg::Bytes(b) => b,
            Arg::Null => return Ok(types::Duckvalue::Null),
        };
        Ok(match which {
            ScalarHandler::Sha1 => types::Duckvalue::Text(sha1_hex(&b)),
            ScalarHandler::Sha512 => types::Duckvalue::Text(sha512_hex(&b)),
            ScalarHandler::Sha3_256 => types::Duckvalue::Text(sha3_256_hex(&b)),
            ScalarHandler::Blake3 => types::Duckvalue::Text(blake3_hex(&b)),
            ScalarHandler::Crc32 => types::Duckvalue::Int64(crc32_of(&b) as i64),
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("crypto: no table functions".into()))
    }
    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("crypto: no aggregates".into()))
    }
    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("crypto: no pragmas".into()))
    }
    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("crypto: no casts".into()))
    }
}

export!(Extension);

// ---- Registration ----

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => return Err(types::Duckerror::Internal("scalar capability returned unexpected variant".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    register_one(&registry, "sha1", types::Logicaltype::Text, det, ScalarHandler::Sha1)?;
    register_one(&registry, "sha512", types::Logicaltype::Text, det, ScalarHandler::Sha512)?;
    register_one(&registry, "sha3_256", types::Logicaltype::Text, det, ScalarHandler::Sha3_256)?;
    register_one(&registry, "blake3", types::Logicaltype::Text, det, ScalarHandler::Blake3)?;
    register_one(&registry, "crc32", types::Logicaltype::Int64, det, ScalarHandler::Crc32)?;
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
    scalar_handlers()
        .lock()
        .expect("scalar handler mutex poisoned")
        .insert(handle, handler);
    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("value".into()),
        logical: types::Logicaltype::Text,
    }];
    let opts = runtime::Funcopts {
        description: Some("cryptographic / checksum hash digest".into()),
        tags: vec!["crypto".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Sha1,
    Sha512,
    Sha3_256,
    Blake3,
    Crc32,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
