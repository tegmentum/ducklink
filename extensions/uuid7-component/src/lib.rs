//! UUIDv7 (RFC 9562, time-ordered) as DuckDB scalars. DETERMINISTIC — the
//! timestamp and the random bits are supplied as arguments, so results are
//! reproducible (no clock, no RNG).
//!
//!   uuid7_build(unix_ms BIGINT, rand_hex VARCHAR) -> VARCHAR
//!       Construct a canonical v7 UUID string from a unix-ms timestamp and a
//!       supplied random hex string. rand_hex fills the 74 random bits
//!       (rand_a: 12 bits, rand_b: 62 bits); it is zero-padded or truncated as
//!       needed. Returns NULL if unix_ms is out of the 48-bit range or rand_hex
//!       is not valid hex.
//!
//!   uuid7_timestamp(uuid VARCHAR) -> BIGINT
//!       Extract the embedded unix-ms timestamp from a v7 UUID; NULL on invalid.
//!
//!   uuid7_is_valid(uuid VARCHAR) -> BOOLEAN
//!       True iff the string is a well-formed v7 UUID (version 7, variant 0b10).
//!
//! Parse failures -> NULL / false. Never panics.

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
            name: "uuid7".into(),
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

fn arg_i64(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        Some(types::Duckvalue::Uint64(v)) => i64::try_from(*v).ok(),
        _ => None,
    }
}

/// Parse a hex string into a 74-bit random field, packed into the low 74 bits
/// of a u128 (zero-padded if short, truncated to the rightmost 74 bits if long).
/// Returns None if any char is not a hex digit.
fn rand_bits_from_hex(hex: &str) -> Option<u128> {
    let mut acc: u128 = 0;
    for c in hex.chars() {
        let nib = c.to_digit(16)?;
        // Shift left a nibble; keep only the low 74 bits (truncate excess).
        acc = (acc << 4) | nib as u128;
        acc &= (1u128 << 74) - 1;
    }
    Some(acc)
}

/// Build the canonical v7 UUID string from a 48-bit timestamp and 74 random bits.
fn build_v7(unix_ms: u64, rand74: u128) -> Option<std::string::String> {
    if unix_ms > 0xFFFF_FFFF_FFFF {
        return None; // does not fit in 48 bits
    }
    let rand_a: u16 = ((rand74 >> 62) & 0xFFF) as u16; // top 12 bits of the 74
    let rand_b: u64 = (rand74 & ((1u128 << 62) - 1)) as u64; // low 62 bits

    let mut bytes = [0u8; 16];
    // 48-bit timestamp, big-endian.
    bytes[0] = (unix_ms >> 40) as u8;
    bytes[1] = (unix_ms >> 32) as u8;
    bytes[2] = (unix_ms >> 24) as u8;
    bytes[3] = (unix_ms >> 16) as u8;
    bytes[4] = (unix_ms >> 8) as u8;
    bytes[5] = unix_ms as u8;
    // version (0x7) | high 4 bits of rand_a
    bytes[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0F);
    bytes[7] = (rand_a & 0xFF) as u8;
    // variant (0b10) | high 6 bits of rand_b
    bytes[8] = 0x80 | ((rand_b >> 56) as u8 & 0x3F);
    bytes[9] = (rand_b >> 48) as u8;
    bytes[10] = (rand_b >> 40) as u8;
    bytes[11] = (rand_b >> 32) as u8;
    bytes[12] = (rand_b >> 24) as u8;
    bytes[13] = (rand_b >> 16) as u8;
    bytes[14] = (rand_b >> 8) as u8;
    bytes[15] = rand_b as u8;

    Some(Uuid::from_bytes(bytes).to_string())
}

/// Is this a well-formed v7 UUID (version nibble 7, variant 0b10)?
fn is_v7(s: &str) -> bool {
    match Uuid::parse_str(s) {
        Ok(u) => {
            let b = u.as_bytes();
            let version = b[6] >> 4;
            let variant = b[8] >> 6;
            version == 7 && variant == 0b10
        }
        Err(_) => false,
    }
}

/// Extract the embedded unix-ms timestamp from a v7 UUID, or None if not a v7.
fn v7_timestamp_ms(s: &str) -> Option<i64> {
    let u = Uuid::parse_str(s).ok()?;
    if !is_v7(s) {
        return None;
    }
    let b = u.as_bytes();
    let ms: u64 = (b[0] as u64) << 40
        | (b[1] as u64) << 32
        | (b[2] as u64) << 24
        | (b[3] as u64) << 16
        | (b[4] as u64) << 8
        | (b[5] as u64);
    i64::try_from(ms).ok()
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
            ScalarHandler::Build => {
                let ms = arg_i64(&args, 0);
                let hex = arg_text(&args, 1);
                match (ms, hex) {
                    (Some(ms), Some(hex)) if ms >= 0 => {
                        match rand_bits_from_hex(&hex)
                            .and_then(|r| build_v7(ms as u64, r))
                        {
                            Some(s) => types::Duckvalue::Text(s.into()),
                            None => types::Duckvalue::Null,
                        }
                    }
                    _ => types::Duckvalue::Null,
                }
            }
            ScalarHandler::Timestamp => {
                match arg_text(&args, 0).and_then(|s| v7_timestamp_ms(&s)) {
                    Some(ms) => types::Duckvalue::Int64(ms),
                    None => types::Duckvalue::Null,
                }
            }
            ScalarHandler::IsValid => match arg_text(&args, 0) {
                Some(s) => types::Duckvalue::Boolean(is_v7(&s)),
                None => types::Duckvalue::Boolean(false),
            },
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuid7: no table functions".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuid7: no aggregates".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuid7: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("uuid7: no casts".into()))
    }
}

export!(Extension);

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
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    register(
        &registry,
        "uuid7_build",
        &[
            ("unix_ms", types::Logicaltype::Int64),
            ("rand_hex", types::Logicaltype::Text),
        ],
        types::Logicaltype::Text,
        det,
        ScalarHandler::Build,
    )?;
    register(
        &registry,
        "uuid7_timestamp",
        &[("uuid", types::Logicaltype::Text)],
        types::Logicaltype::Int64,
        det,
        ScalarHandler::Timestamp,
    )?;
    register(
        &registry,
        "uuid7_is_valid",
        &[("uuid", types::Logicaltype::Text)],
        types::Logicaltype::Boolean,
        det,
        ScalarHandler::IsValid,
    )?;
    Ok(())
}

fn register(
    registry: &runtime::ScalarRegistry,
    name: &str,
    args: &[(&str, types::Logicaltype)],
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
    let func_args: std::vec::Vec<runtime::Funcarg> = args
        .iter()
        .map(|(n, t)| runtime::Funcarg {
            name: Some((*n).into()),
            logical: t.clone(),
        })
        .collect();
    let opts = runtime::Funcopts {
        description: Some("UUIDv7 (RFC 9562) helper".into()),
        tags: vec!["uuid".into(), "uuid7".into()],
        attributes,
    };
    registry.register(name, &func_args, &returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Build,
    Timestamp,
    IsValid,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
