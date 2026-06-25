//! t-digest quantile estimation as a DuckDB AGGREGATE + query scalars
//! (not in DuckDB core):
//!   tdigest(value DOUBLE) AGGREGATE -> BLOB (a serialized t-digest sketch over
//!     the aggregated non-NULL doubles; empty input yields NULL),
//!   tdigest_quantile(digest BLOB, q DOUBLE) -> DOUBLE (estimate the q-quantile,
//!     q in [0,1], from the serialized sketch),
//!   tdigest_count(digest BLOB) -> BIGINT (total count of values in the sketch).
//! NULL inputs are skipped on build; a malformed/NULL digest blob yields NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use tdigest::TDigest;

struct Extension;

/// Accept DOUBLE (float64) values; also tolerate integer inputs so the
/// aggregate is usable over INT columns without an explicit cast.
fn float64(v: &types::Duckvalue) -> Option<f64> {
    match v {
        types::Duckvalue::Float64(f) => Some(*f),
        types::Duckvalue::Int64(i) => Some(*i as f64),
        types::Duckvalue::Uint64(u) => Some(*u as f64),
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

/// Deserialize a t-digest from a blob, returning None on any malformed input.
fn decode(v: Option<&types::Duckvalue>) -> Option<TDigest> {
    let bytes = v.and_then(blob)?;
    bincode::deserialize::<TDigest>(bytes).ok()
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register()?;
        Ok(types::Loadresult {
            name: "tdigest".into(),
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
        match handle {
            // tdigest_quantile(digest BLOB, q DOUBLE) -> DOUBLE
            10 => {
                let digest = match decode(args.first()) {
                    Some(d) => d,
                    None => return Ok(types::Duckvalue::Null),
                };
                let q = match args.get(1).and_then(float64) {
                    Some(q) if (0.0..=1.0).contains(&q) => q,
                    _ => return Ok(types::Duckvalue::Null),
                };
                if digest.is_empty() {
                    return Ok(types::Duckvalue::Null);
                }
                let est = digest.estimate_quantile(q);
                if est.is_finite() {
                    Ok(types::Duckvalue::Float64(est))
                } else {
                    Ok(types::Duckvalue::Null)
                }
            }
            // tdigest_count(digest BLOB) -> BIGINT
            11 => {
                let digest = match decode(args.first()) {
                    Some(d) => d,
                    None => return Ok(types::Duckvalue::Null),
                };
                Ok(types::Duckvalue::Int64(digest.count() as i64))
            }
            _ => Err(types::Duckerror::Internal("unknown scalar handle".into())),
        }
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
        Err(types::Duckerror::Unsupported("tdigest: no table fns".into()))
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
        // Collect the non-NULL doubles to build the digest over.
        let mut values: std::vec::Vec<f64> = std::vec::Vec::new();
        for row in &rows {
            if let Some(x) = row.first().and_then(float64) {
                if x.is_finite() {
                    values.push(x);
                }
            }
        }
        if values.is_empty() {
            // Nothing to build over -> NULL digest.
            return Ok(types::Duckvalue::Null);
        }
        let digest = TDigest::new_with_size(100).merge_unsorted(values);
        let bytes = match bincode::serialize(&digest) {
            Ok(b) => b,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("tdigest serialize failed: {e}").into(),
                ))
            }
        };
        Ok(types::Duckvalue::Blob(bytes.into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tdigest: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tdigest: no casts".into()))
    }
}

export!(Extension);

fn register() -> Result<(), types::Duckerror> {
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    // aggregate: tdigest(value DOUBLE) -> BLOB
    let acap = runtime::get_capability(types::Capabilitykind::Aggregate)
        .ok_or_else(|| types::Duckerror::Internal("no aggregate capability".into()))?;
    let areg = match acap {
        runtime::Capability::Aggregate(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    areg.register(
        "tdigest",
        &[runtime::Funcarg {
            name: Some("value".into()),
            logical: types::Logicaltype::Float64,
        }],
        &types::Logicaltype::Blob,
        runtime::AggregateCallback::new(1),
        Some(&runtime::Funcopts {
            description: Some("build a t-digest quantile sketch".into()),
            tags: vec!["sketch".into()],
            attributes: det,
        }),
    )?;
    // scalars
    let scap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let sreg = match scap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    // tdigest_quantile(digest BLOB, q DOUBLE) -> DOUBLE
    sreg.register(
        "tdigest_quantile",
        &[
            runtime::Funcarg {
                name: Some("digest".into()),
                logical: types::Logicaltype::Blob,
            },
            runtime::Funcarg {
                name: Some("q".into()),
                logical: types::Logicaltype::Float64,
            },
        ],
        &types::Logicaltype::Float64,
        runtime::ScalarCallback::new(10),
        Some(&runtime::Funcopts {
            description: Some("estimate the q-quantile from a t-digest".into()),
            tags: vec!["sketch".into()],
            attributes: det,
        }),
    )?;
    // tdigest_count(digest BLOB) -> BIGINT
    sreg.register(
        "tdigest_count",
        &[runtime::Funcarg {
            name: Some("digest".into()),
            logical: types::Logicaltype::Blob,
        }],
        &types::Logicaltype::Int64,
        runtime::ScalarCallback::new(11),
        Some(&runtime::Funcopts {
            description: Some("total count of values in a t-digest".into()),
            tags: vec!["sketch".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}
