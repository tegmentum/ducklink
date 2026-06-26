//! `ml_kmeans(x DOUBLE, y DOUBLE, k INTEGER) -> VARCHAR` — a DuckDB AGGREGATE
//! that computes k-means clustering via Python/numpy on a SINGLE resident,
//! SHARED `compose:dynlink` pylon provider.
//!
//! The aggregate accumulates the (x, y) rows and captures k, then at finalize:
//!   1. msgpack-encodes `{"points": [[x,y],...], "k": k}`,
//!   2. `resolve-by-id("pylon")` on the host's `compose:dynlink/linker`,
//!   3. `invoke("kmeans", payload)` — forwarded verbatim to the resident
//!      pylon's `compose:dynlink/endpoint.handle`, which runs numpy k-means,
//!   4. msgpack-decodes `{"centroids": [[cx,cy],...]}`,
//!   5. formats the centroids as a JSON string and returns it as VARCHAR.
//!
//! The pylon is instantiated ONCE by the host and shared across every
//! ml_kmeans call (and every other compose:dynlink guest): one warmed ~38 MB
//! pylon serving DuckDB ML functions. Adding more ML functions (linreg, ...)
//! reduces to a Python method in the provider + a thin aggregate like this.
use serde::{Deserialize, Serialize};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "mlkmeans", generate_all });

use compose::dynlink::linker;
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

/// The provider id registered in the host (DUCKLINK_PROVIDERS=pylon=...).
const PYLON_ID: &str = "pylon";
/// The aggregate callback handle (matches the registration below).
const ML_KMEANS_HANDLE: u32 = 1;

/// Accept DOUBLE; tolerate integer inputs so the aggregate works over INT
/// columns without an explicit cast.
fn float64(v: &types::Duckvalue) -> Option<f64> {
    match v {
        types::Duckvalue::Float64(f) => Some(*f),
        types::Duckvalue::Int64(i) => Some(*i as f64),
        types::Duckvalue::Uint64(u) => Some(*u as f64),
        types::Duckvalue::Float32(f) => Some(*f as f64),
        types::Duckvalue::Int32(i) => Some(*i as f64),
        _ => None,
    }
}

/// Accept an integer-ish k.
fn int_k(v: &types::Duckvalue) -> Option<i64> {
    match v {
        types::Duckvalue::Int32(i) => Some(*i as i64),
        types::Duckvalue::Int64(i) => Some(*i),
        types::Duckvalue::Uint32(u) => Some(*u as i64),
        types::Duckvalue::Uint64(u) => Some(*u as i64),
        types::Duckvalue::Float64(f) => Some(*f as i64),
        _ => None,
    }
}

/// The msgpack request the pylon's `kmeans` method expects:
/// `{"points": [[x,y],...], "k": int}`.
#[derive(Serialize)]
struct KmeansRequest {
    points: std::vec::Vec<[f64; 2]>,
    k: i64,
}

/// The msgpack response: `{"centroids": [[cx,cy],...]}`.
#[derive(Deserialize)]
struct KmeansResponse {
    centroids: std::vec::Vec<std::vec::Vec<f64>>,
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register()?;
        Ok(types::Loadresult {
            name: "mlkmeans".into(),
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
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mlkmeans: no scalars".into()))
    }
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mlkmeans: no scalars".into()))
    }
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mlkmeans: no table fns".into()))
    }
    fn call_aggregate(
        handle: u32,
        rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != ML_KMEANS_HANDLE {
            return Err(types::Duckerror::Internal(
                "unknown aggregate handle".into(),
            ));
        }
        // Accumulate (x, y) rows and capture k. Each row is [x, y, k].
        let mut points: std::vec::Vec<[f64; 2]> = std::vec::Vec::new();
        let mut k: Option<i64> = None;
        for row in &rows {
            let x = row.first().and_then(float64);
            let y = row.get(1).and_then(float64);
            if let Some(kv) = row.get(2).and_then(int_k) {
                // k is constant per group; last non-NULL wins.
                k = Some(kv);
            }
            if let (Some(x), Some(y)) = (x, y) {
                if x.is_finite() && y.is_finite() {
                    points.push([x, y]);
                }
            }
        }
        if points.is_empty() {
            return Ok(types::Duckvalue::Null);
        }
        let k = match k {
            Some(k) if k > 0 => k,
            _ => return Ok(types::Duckvalue::Null),
        };

        // 1. msgpack-encode the request (standard msgpack map, matching the
        //    provider's _msgpack.py wire format).
        let req = KmeansRequest { points, k };
        let payload = match rmp_serde::to_vec_named(&req) {
            Ok(p) => p,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("mlkmeans: msgpack encode failed: {e}").into(),
                ))
            }
        };

        // 2. resolve the shared, resident pylon provider.
        let inst = match linker::resolve_by_id(PYLON_ID) {
            Ok(i) => i,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!(
                        "mlkmeans: resolve provider '{PYLON_ID}' failed: {} (is DUCKLINK_PROVIDERS=pylon=... set?)",
                        e.message
                    )
                    .into(),
                ))
            }
        };

        // 3. invoke("kmeans", payload) -> opaque bytes (numpy k-means on the
        //    resident pylon).
        let resp_bytes = match inst.invoke("kmeans", &payload) {
            Ok(b) => b,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("mlkmeans: pylon kmeans invoke failed: {}", e.message).into(),
                ))
            }
        };

        // 4. msgpack-decode {"centroids": [[cx,cy],...]}.
        let resp: KmeansResponse = match rmp_serde::from_slice(&resp_bytes) {
            Ok(r) => r,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("mlkmeans: msgpack decode failed: {e}").into(),
                ))
            }
        };

        // 5. format the centroids as a JSON string -> VARCHAR.
        let json = match serde_json::to_string(&resp.centroids) {
            Ok(s) => s,
            Err(e) => {
                return Err(types::Duckerror::Internal(
                    std::format!("mlkmeans: json format failed: {e}").into(),
                ))
            }
        };
        Ok(types::Duckvalue::Text(json.into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mlkmeans: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mlkmeans: no casts".into()))
    }
}

export!(Extension);

fn register() -> Result<(), types::Duckerror> {
    let det = types::Funcflags::STATELESS;
    // aggregate: ml_kmeans(x DOUBLE, y DOUBLE, k INTEGER) -> VARCHAR
    let acap = runtime::get_capability(types::Capabilitykind::Aggregate)
        .ok_or_else(|| types::Duckerror::Internal("no aggregate capability".into()))?;
    let areg = match acap {
        runtime::Capability::Aggregate(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    areg.register(
        "ml_kmeans",
        &[
            runtime::Funcarg {
                name: Some("x".into()),
                logical: types::Logicaltype::Float64,
            },
            runtime::Funcarg {
                name: Some("y".into()),
                logical: types::Logicaltype::Float64,
            },
            runtime::Funcarg {
                name: Some("k".into()),
                logical: types::Logicaltype::Int32,
            },
        ],
        &types::Logicaltype::Text,
        runtime::AggregateCallback::new(ML_KMEANS_HANDLE),
        Some(&runtime::Funcopts {
            description: Some(
                "k-means centroids via numpy on the shared compose:dynlink pylon".into(),
            ),
            tags: vec!["ml".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}
