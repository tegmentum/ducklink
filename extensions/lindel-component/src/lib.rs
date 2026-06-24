//! Space-filling curve linearization as DuckDB scalars (hand-rolled bit math):
//!   morton_encode(x, y) -> bigint, morton_decode_x / morton_decode_y(z) -> bigint,
//!   hilbert_encode(x, y) -> bigint, hilbert_decode_x / hilbert_decode_y(h) -> bigint.
//!
//! Both curves operate on 2D coordinates whose components are the low 32 bits of
//! the input (range 0 .. 2^32-1), producing a 64-bit index. Encode/decode round-
//! trip exactly. Negative or out-of-range input -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

const MAX_COORD: i64 = 0xFFFF_FFFF; // 2^32 - 1, the largest 32-bit component.

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "lindel".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

// ---- Morton (Z-order) bit interleave for a single 32-bit lane. ----
fn part1by1(n: u64) -> u64 {
    let mut x = n & 0x0000_0000_FFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

fn compact1by1(n: u64) -> u64 {
    let mut x = n & 0x5555_5555_5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
    x = (x | (x >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x >> 4)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
    x
}

fn morton_encode(x: u64, y: u64) -> u64 {
    part1by1(x) | (part1by1(y) << 1)
}

fn morton_decode(z: u64) -> (u64, u64) {
    (compact1by1(z), compact1by1(z >> 1))
}

// ---- Hilbert curve (2D, 32 bits per component => 64-bit index). ----
const HILBERT_BITS: u32 = 32;

fn hilbert_encode(x: u64, y: u64) -> u64 {
    let mut rx: u64;
    let mut ry: u64;
    let mut x = x;
    let mut y = y;
    let mut d: u64 = 0;
    let mut s: u64 = 1u64 << (HILBERT_BITS - 1);
    while s > 0 {
        rx = if (x & s) > 0 { 1 } else { 0 };
        ry = if (y & s) > 0 { 1 } else { 0 };
        d = d.wrapping_add(s.wrapping_mul(s).wrapping_mul((3 * rx) ^ ry));
        // rotate
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

fn hilbert_decode(h: u64) -> (u64, u64) {
    let mut rx: u64;
    let mut ry: u64;
    let mut t: u64 = h;
    let mut x: u64 = 0;
    let mut y: u64 = 0;
    let mut s: u64 = 1;
    let n: u64 = 1u64 << HILBERT_BITS;
    while s < n {
        rx = 1 & (t / 2);
        ry = 1 & (t ^ rx);
        // rotate
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        x = x.wrapping_add(s.wrapping_mul(rx));
        y = y.wrapping_add(s.wrapping_mul(ry));
        t /= 4;
        s <<= 1;
    }
    (x, y)
}

// ---- argument coercion ----
fn coord_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        _ => None,
    }
}

/// Validate a coordinate component is in [0, 2^32-1]; else None (-> NULL).
fn checked_coord(v: i64) -> Option<u64> {
    if (0..=MAX_COORD).contains(&v) { Some(v as u64) } else { None }
}

/// Validate an index value is non-negative (the encoded outputs are <= i64::MAX
/// since each component is <= 2^32-1, so the 64-bit index never sets bit 63).
fn checked_index(v: i64) -> Option<u64> {
    if v >= 0 { Some(v as u64) } else { None }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }

    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            F::MortonEncode | F::HilbertEncode => {
                match (coord_arg(&args, 0), coord_arg(&args, 1)) {
                    (Some(x), Some(y)) => match (checked_coord(x), checked_coord(y)) {
                        (Some(x), Some(y)) => {
                            let z = if which == F::MortonEncode { morton_encode(x, y) } else { hilbert_encode(x, y) };
                            types::Duckvalue::Int64(z as i64)
                        }
                        _ => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            F::MortonDecodeX | F::MortonDecodeY => {
                match coord_arg(&args, 0).and_then(checked_index) {
                    Some(z) => {
                        let (x, y) = morton_decode(z);
                        types::Duckvalue::Int64(if which == F::MortonDecodeX { x as i64 } else { y as i64 })
                    }
                    None => types::Duckvalue::Null,
                }
            }
            F::HilbertDecodeX | F::HilbertDecodeY => {
                match coord_arg(&args, 0).and_then(checked_index) {
                    Some(h) => {
                        let (x, y) = hilbert_decode(h);
                        types::Duckvalue::Int64(if which == F::HilbertDecodeX { x as i64 } else { y as i64 })
                    }
                    None => types::Duckvalue::Null,
                }
            }
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("lindel: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("lindel: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("lindel: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("lindel: no casts".into())) }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let bigint = || types::Logicaltype::Int64;
    let xy_args = || vec![
        runtime::Funcarg { name: Some("x".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("y".into()), logical: types::Logicaltype::Int64 },
    ];
    let one_arg = |name: &str| vec![
        runtime::Funcarg { name: Some(name.into()), logical: types::Logicaltype::Int64 },
    ];

    // encoders: (x, y) -> bigint
    for (name, f, desc) in [
        ("morton_encode", F::MortonEncode, "2D Z-order (Morton) interleave of low 32 bits of x, y"),
        ("hilbert_encode", F::HilbertEncode, "2D Hilbert curve index of x, y"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, f);
        reg.register(name, &xy_args(), bigint(), runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["lindel".into()], attributes: det }))?;
    }

    // decoders: (index) -> bigint
    for (name, f, arg, desc) in [
        ("morton_decode_x", F::MortonDecodeX, "z", "x component of a Morton index"),
        ("morton_decode_y", F::MortonDecodeY, "z", "y component of a Morton index"),
        ("hilbert_decode_x", F::HilbertDecodeX, "h", "x component of a Hilbert index"),
        ("hilbert_decode_y", F::HilbertDecodeY, "h", "y component of a Hilbert index"),
    ] {
        let hd = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(hd, f);
        reg.register(name, &one_arg(arg), bigint(), runtime::ScalarCallback::new(hd),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["lindel".into()], attributes: det }))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum F { MortonEncode, MortonDecodeX, MortonDecodeY, HilbertEncode, HilbertDecodeX, HilbertDecodeY }

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
