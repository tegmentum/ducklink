//! Classic hex dumps of bytes as DuckDB scalars:
//!   hexdump(data BLOB)   -> VARCHAR  canonical `hexdump -C` / `xxd` style dump.
//!   hex_pretty(data BLOB) -> VARCHAR space-separated hex bytes ("de ad be ef").
//! NULL input -> NULL. Never panics.
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

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "hexdump".into(),
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

/// Borrow the bytes of arg `i` as a BLOB (or treat TEXT as its UTF-8 bytes).
/// Returns `None` for SQL NULL so callers can short-circuit to NULL output.
fn blob_arg(args: &[types::Duckvalue], i: usize) -> Option<std::vec::Vec<u8>> {
    match args.get(i) {
        Some(types::Duckvalue::Blob(b)) => Some(b.to_vec()),
        Some(types::Duckvalue::Text(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

/// `xxd` / `hexdump -C`: OFFSET(8 hex)  16 hex bytes (8|8 grouped)  |ascii|.
fn canonical_dump(data: &[u8]) -> std::string::String {
    let mut out = std::string::String::new();
    for (line, chunk) in data.chunks(16).enumerate() {
        let offset = line * 16;
        out.push_str(&format!("{:08x}  ", offset));
        // 16 hex byte slots, grouped 8|8, padding missing bytes with spaces.
        for col in 0..16 {
            if col == 8 {
                out.push(' ');
            }
            match chunk.get(col) {
                Some(b) => out.push_str(&format!("{:02x} ", b)),
                None => out.push_str("   "),
            }
        }
        out.push('|');
        for &b in chunk {
            out.push(if (0x20..=0x7e).contains(&b) { b as char } else { '.' });
        }
        out.push('|');
        out.push('\n');
    }
    out
}

/// Space-separated lowercase hex bytes: "de ad be ef".
fn pretty_hex(data: &[u8]) -> std::string::String {
    let mut out = std::string::String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02x}", b));
    }
    out
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
        Ok(match blob_arg(&args, 0) {
            None => types::Duckvalue::Null,
            Some(bytes) => {
                let s = match which {
                    H::Dump => canonical_dump(&bytes),
                    H::Pretty => pretty_hex(&bytes),
                };
                types::Duckvalue::Text(s.into())
            }
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hexdump: no table fns".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hexdump: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hexdump: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hexdump: no casts".into()))
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
    for (name, h, desc) in [
        ("hexdump", H::Dump, "BLOB -> canonical hex+ASCII dump (xxd / hexdump -C)"),
        ("hex_pretty", H::Pretty, "BLOB -> space-separated hex bytes"),
    ] {
        let handle = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(handle, h);
        reg.register(
            name,
            &[runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Blob }],
            types::Logicaltype::Text,
            runtime::ScalarCallback::new(handle),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["encoding".into()],
                attributes: det,
            }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum H {
    Dump,
    Pretty,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
