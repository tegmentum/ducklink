//! Read PCAP / PCAPNG capture files as a DuckDB table function over an
//! in-memory BLOB.
//!
//!   read_pcap(data BLOB) -> table(idx BIGINT, ts_sec BIGINT, ts_usec BIGINT,
//!                                 caplen BIGINT, origlen BIGINT, data BLOB)
//!
//! One row per captured packet. `idx` is 1-indexed; `ts_sec`/`ts_usec` are the
//! packet timestamp (for PCAPNG these are the raw high/low timestamp halves);
//! `caplen`/`origlen` are the captured and original (on-wire) lengths; `data`
//! is the raw captured packet bytes (link-layer frame). Both classic PCAP and
//! PCAPNG inputs are accepted. A malformed blob yields zero rows -- never a
//! panic.
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use pcap_parser::{create_reader, PcapBlockOwned, PcapError};
use pcap_parser::pcapng::Block;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_read_pcap()?;
        Ok(types::Loadresult {
            name: "pcap".into(),
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

impl callback_dispatch::Guest for Extension {
    // major-4 columnar dispatch: pcap is table-only, so the columnar hot
    // methods are Unsupported stubs. The hand-written call_table is unchanged.
    datalink_extcore::columnar_stub!();
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("pcap: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // single registered table fn; any handle maps to read_pcap
        let _ = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        let bytes: std::vec::Vec<u8> = match args.into_iter().next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            // accept TEXT too, so `read_pcap(<varchar>)` degrades gracefully
            Some(types::Duckvalue::Text(s)) => s.into_bytes(),
            Some(types::Duckvalue::Null) | None => return Ok(Vec::new().into()),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "read_pcap expects a single BLOB argument".into(),
                ))
            }
        };

        Ok(parse(&bytes).into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("pcap: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("pcap: no casts".into()))
    }
}

export!(Extension);

/// One packet row: (idx, ts_sec, ts_usec, caplen, origlen, data).
fn row(
    idx: i64,
    ts_sec: i64,
    ts_usec: i64,
    caplen: i64,
    origlen: i64,
    data: &[u8],
) -> std::vec::Vec<types::Duckvalue> {
    vec![
        types::Duckvalue::Int64(idx),
        types::Duckvalue::Int64(ts_sec),
        types::Duckvalue::Int64(ts_usec),
        types::Duckvalue::Int64(caplen),
        types::Duckvalue::Int64(origlen),
        types::Duckvalue::Blob(data.to_vec().into()),
    ]
}

/// Parse the capture bytes (PCAP or PCAPNG) and emit one row per packet.
/// Returns an empty result on any malformed input rather than panicking.
fn parse(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::new();

    let mut reader = match create_reader(65536, Cursor::new(bytes)) {
        Ok(r) => r,
        Err(_) => return out,
    };

    let mut idx: i64 = 0;
    loop {
        match reader.next() {
            Ok((offset, block)) => {
                match block {
                    PcapBlockOwned::Legacy(b) => {
                        idx += 1;
                        out.push(row(
                            idx,
                            b.ts_sec as i64,
                            b.ts_usec as i64,
                            b.caplen as i64,
                            b.origlen as i64,
                            b.data,
                        ));
                    }
                    PcapBlockOwned::NG(Block::EnhancedPacket(ep)) => {
                        idx += 1;
                        out.push(row(
                            idx,
                            ep.ts_high as i64,
                            ep.ts_low as i64,
                            ep.caplen as i64,
                            ep.origlen as i64,
                            ep.data,
                        ));
                    }
                    PcapBlockOwned::NG(Block::SimplePacket(sp)) => {
                        idx += 1;
                        let len = sp.origlen as i64;
                        out.push(row(idx, 0, 0, sp.data.len() as i64, len, sp.data));
                    }
                    // headers, interface descriptions, name resolution, etc.
                    _ => {}
                }
                reader.consume(offset);
            }
            Err(PcapError::Eof) => break,
            Err(PcapError::Incomplete(_)) => {
                if reader.refill().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    out
}

fn register_read_pcap() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::ReadPcap);

    let args = vec![runtime::Funcarg {
        name: Some("data".into()),
        logical: types::Logicaltype::Blob,
    }];
    let columns = vec![
        types::Columndef {
            name: "idx".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "ts_sec".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "ts_usec".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "caplen".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "origlen".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "data".into(),
            logical: types::Logicaltype::Blob,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Read PCAP/PCAPNG capture bytes into per-packet (idx, ts_sec, ts_usec, caplen, origlen, data) rows".into(),
        ),
        tags: vec!["pcap".into(), "pcapng".into(), "network".into()],
    };
    reg.register("read_pcap", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    ReadPcap,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
