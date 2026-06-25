//! Xor approximate-membership filter as a DuckDB AGGREGATE + query scalar
//! (not in DuckDB core):
//!   xor_filter(value BIGINT) AGGREGATE -> BLOB (a serialized Xor8 filter over
//!     the distinct aggregated u64 values; no false negatives),
//!   xor_filter_contains(filter BLOB, value BIGINT) -> BOOLEAN (probabilistic
//!     membership; ~0.4% false-positive rate, never a false negative).
//! NULL inputs are skipped on build; a malformed/NULL filter blob yields NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use std::convert::TryFrom;
use xorf::{Filter, Xor8};

struct Extension;

/// BIGINT values are signed i64; reinterpret the bits as u64 so the filter keys
/// are stable and lossless (the same value reinterprets back identically on the
/// query side).
fn int64(v: &types::Duckvalue) -> Option<u64> {
    match v {
        types::Duckvalue::Int64(i) => Some(*i as u64),
        types::Duckvalue::Uint64(u) => Some(*u),
        _ => None,
    }
}

fn blob(v: &types::Duckvalue) -> Option<&[u8]> {
    if let types::Duckvalue::Blob(b) = v {
        Some(b)
    } else {
        None
    }
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register()?;
        Ok(types::Loadresult {
            name: "bitfilters".into(),
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
    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != 10 {
            return Err(types::Duckerror::Internal("unknown scalar handle".into()));
        }
        // Deserialize the filter; return NULL safely on any malformed/NULL blob.
        let bytes = match args.first().and_then(blob) {
            Some(b) => b,
            None => return Ok(types::Duckvalue::Null),
        };
        let filter: Xor8 = match bincode::deserialize(bytes) {
            Ok(f) => f,
            Err(_) => return Ok(types::Duckvalue::Null),
        };
        let key = match args.get(1).and_then(int64) {
            Some(k) => k,
            None => return Ok(types::Duckvalue::Null),
        };
        Ok(types::Duckvalue::Boolean(filter.contains(&key)))
    }
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
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
            )?);
        }
        Ok(out)
    }
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "bitfilters: no table fns".into(),
        ))
    }
    fn call_aggregate(
        handle: u32,
        rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != 1 {
            return Err(types::Duckerror::Internal(
                "unknown aggregate handle".into(),
            ));
        }
        // Collect the distinct non-NULL keys. Xor8 construction requires keys
        // to be distinct, so dedup here.
        let mut keys: std::vec::Vec<u64> = std::vec::Vec::new();
        for row in &rows {
            if let Some(k) = row.first().and_then(int64) {
                keys.push(k);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        if keys.is_empty() {
            // Nothing to build over -> NULL filter.
            return Ok(types::Duckvalue::Null);
        }
        let filter = match Xor8::try_from(&keys) {
            Ok(f) => f,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("xor filter build failed: {e:?}").into(),
                ))
            }
        };
        let bytes = match bincode::serialize(&filter) {
            Ok(b) => b,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("xor filter serialize failed: {e}").into(),
                ))
            }
        };
        Ok(types::Duckvalue::Blob(bytes.into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "bitfilters: no pragmas".into(),
        ))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("bitfilters: no casts".into()))
    }
}

export!(Extension);

fn register() -> Result<(), types::Duckerror> {
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    // aggregate: xor_filter(value BIGINT) -> BLOB
    let acap = runtime::get_capability(types::Capabilitykind::Aggregate)
        .ok_or_else(|| types::Duckerror::Internal("no aggregate capability".into()))?;
    let areg = match acap {
        runtime::Capability::Aggregate(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    areg.register(
        "xor_filter",
        &[runtime::Funcarg {
            name: Some("value".into()),
            logical: types::Logicaltype::Int64,
        }],
        &types::Logicaltype::Blob,
        runtime::AggregateCallback::new(1),
        Some(&runtime::Funcopts {
            description: Some("build an xor approximate-membership filter".into()),
            tags: vec!["sketch".into()],
            attributes: det,
        }),
    )?;
    // scalar: xor_filter_contains(filter BLOB, value BIGINT) -> BOOLEAN
    let scap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let sreg = match scap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    sreg.register(
        "xor_filter_contains",
        &[
            runtime::Funcarg {
                name: Some("filter".into()),
                logical: types::Logicaltype::Blob,
            },
            runtime::Funcarg {
                name: Some("value".into()),
                logical: types::Logicaltype::Int64,
            },
        ],
        &types::Logicaltype::Boolean,
        runtime::ScalarCallback::new(10),
        Some(&runtime::Funcopts {
            description: Some("xor filter membership".into()),
            tags: vec!["sketch".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}
