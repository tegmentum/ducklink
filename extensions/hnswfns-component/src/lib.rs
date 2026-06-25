//! Custom HNSW vector index (Item 3 / M2a).
//!
//! Two faces over one shared, in-component index state (keyed by index NAME):
//!
//!   (a) the `index` / `index-dispatch` WIT surface: the wasm DuckDB core drives
//!       `CREATE INDEX ... USING wasm_hnsw (e)` into create -> append -> build,
//!       which we turn into an `instant-distance` HNSW map, AND
//!   (b) a `hnsw_search(index_name VARCHAR, query VARCHAR, k BIGINT) -> table`
//!       table function that runs an explicit kNN over the SAME built map (so the
//!       milestone is demonstrable without the optimizer auto-rewrite, which is
//!       M2b). `query` is a JSON array of floats, e.g. '[1,2,2]'.
//!
//! The HNSW graph is built with the pure-Rust `instant-distance` crate (compiled
//! to wasm). State is keyed by index NAME so both faces reach the same index; a
//! handle->name map lets the core-driven build path (which uses opaque handles)
//! and the table-fn path (which uses names) converge.
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-index" });

use duckdb::extension::{index, runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest, index_dispatch};

use instant_distance::{Builder, HnswMap, Point, Search};

/// Opaque handle for the single registered `hnsw_search` table function.
const TABLE_HANDLE: u32 = 1;

struct Extension;

// ---------------------------------------------------------------------------
// HNSW point: a Vec<f32> with squared-Euclidean distance.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct VecPoint(std::vec::Vec<f32>);

impl Point for VecPoint {
    fn distance(&self, other: &Self) -> f32 {
        // Euclidean distance. instant-distance ranks by this value, and we also
        // surface it as the `distance` column -- matching array_distance (L2).
        let mut sum = 0.0f32;
        let n = self.0.len().min(other.0.len());
        for i in 0..n {
            let d = self.0[i] - other.0[i];
            sum += d * d;
        }
        sum.sqrt()
    }
}

/// One index: either accumulating rows (pre-build) or a finalized HNSW map.
struct IndexState {
    dims: usize,
    // Accumulation buffers (consumed at build).
    rowids: std::vec::Vec<i64>,
    points: std::vec::Vec<VecPoint>,
    // The finalized map (None until index-build). values = rowids.
    map: Option<HnswMap<VecPoint, i64>>,
}

impl IndexState {
    fn new(dims: usize) -> Self {
        IndexState {
            dims,
            rowids: std::vec::Vec::new(),
            points: std::vec::Vec::new(),
            map: None,
        }
    }
}

thread_local! {
    /// Index state keyed by index NAME (the convergence point for both faces).
    static INDEXES: RefCell<HashMap<std::string::String, IndexState>> = RefCell::new(HashMap::new());
    /// handle -> index name, so the core's opaque-handle build path resolves to
    /// the same named index the table function searches.
    static HANDLE_NAMES: RefCell<HashMap<u32, std::string::String>> = RefCell::new(HashMap::new());
    static NEXT_HANDLE: RefCell<u32> = const { RefCell::new(1) };
}

fn handle_name(handle: u32) -> Result<std::string::String, types::Duckerror> {
    HANDLE_NAMES.with(|m| {
        m.borrow()
            .get(&handle)
            .cloned()
            .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown index handle {handle}")))
    })
}

// ---------------------------------------------------------------------------
// guest: load() registers the index type + the hnsw_search table function.
// ---------------------------------------------------------------------------

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        index::register_index_type("wasm_hnsw")?;
        register_hnsw_search()?;
        Ok(types::Loadresult {
            name: "hnswfns".into(),
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

// ---------------------------------------------------------------------------
// index-dispatch: build (create/append/build) + drop. The core drives this for
// CREATE INDEX. search() is implemented but the core routes search through the
// table function instead; it is kept so the WIT contract is complete.
// ---------------------------------------------------------------------------

impl index_dispatch::Guest for Extension {
    fn index_create(
        _type_name: String,
        index_name: String,
        dims: u32,
    ) -> Result<u32, types::Duckerror> {
        let name = index_name.to_string();
        INDEXES.with(|m| {
            m.borrow_mut().insert(name.clone(), IndexState::new(dims as usize));
        });
        let handle = NEXT_HANDLE.with(|n| {
            let mut n = n.borrow_mut();
            let h = *n;
            *n += 1;
            h
        });
        HANDLE_NAMES.with(|m| m.borrow_mut().insert(handle, name));
        Ok(handle)
    }

    fn index_append(
        handle: u32,
        rowids: Vec<i64>,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), types::Duckerror> {
        let name = handle_name(handle)?;
        if rowids.len() != vectors.len() {
            return Err(types::Duckerror::Invalidargument(
                "index_append: rowids and vectors length mismatch".into(),
            ));
        }
        INDEXES.with(|m| {
            let mut m = m.borrow_mut();
            let st = m
                .get_mut(&name)
                .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown index '{name}'")))?;
            for (rid, v) in rowids.iter().zip(vectors.iter()) {
                let pt: std::vec::Vec<f32> = v.iter().copied().collect();
                if pt.len() != st.dims {
                    return Err(types::Duckerror::Invalidargument(format!(
                        "index_append: vector length {} != index dims {}",
                        pt.len(),
                        st.dims
                    )));
                }
                st.rowids.push(*rid);
                st.points.push(VecPoint(pt));
            }
            Ok(())
        })
    }

    fn index_build(handle: u32) -> Result<(), types::Duckerror> {
        let name = handle_name(handle)?;
        INDEXES.with(|m| {
            let mut m = m.borrow_mut();
            let st = m
                .get_mut(&name)
                .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown index '{name}'")))?;
            let points = std::mem::take(&mut st.points);
            let values = std::mem::take(&mut st.rowids);
            // Deterministic seed (no entropy dependency); ef defaults are fine
            // for the small M2a vectors.
            let map = Builder::default().seed(1).build(points, values);
            st.map = Some(map);
            Ok(())
        })
    }

    fn index_search(
        handle: u32,
        query: Vec<f32>,
        k: u32,
    ) -> Result<Vec<index_dispatch::IndexHit>, types::Duckerror> {
        let name = handle_name(handle)?;
        let q: std::vec::Vec<f32> = query.iter().copied().collect();
        search_named(&name, &q, k)
    }

    fn index_drop(handle: u32) -> Result<(), types::Duckerror> {
        if let Ok(name) = handle_name(handle) {
            INDEXES.with(|m| {
                m.borrow_mut().remove(&name);
            });
        }
        HANDLE_NAMES.with(|m| {
            m.borrow_mut().remove(&handle);
        });
        Ok(())
    }
}

/// Run a kNN search against the built map named `name`. Shared by index-search
/// and the hnsw_search table function.
fn search_named(
    name: &str,
    query: &[f32],
    k: u32,
) -> Result<Vec<index_dispatch::IndexHit>, types::Duckerror> {
    INDEXES.with(|m| {
        let m = m.borrow();
        let st = m
            .get(name)
            .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown index '{name}'")))?;
        let map = st.map.as_ref().ok_or_else(|| {
            types::Duckerror::Invalidstate(format!("index '{name}' has not been built"))
        })?;
        let qp = VecPoint(query.to_vec());
        let mut search = Search::default();
        let mut hits: std::vec::Vec<index_dispatch::IndexHit> = std::vec::Vec::new();
        for item in map.search(&qp, &mut search).take(k as usize) {
            hits.push(index_dispatch::IndexHit {
                rowid: *item.value,
                distance: item.distance,
            });
        }
        Ok(hits.into())
    })
}

// ---------------------------------------------------------------------------
// callback-dispatch: the hnsw_search table function.
// ---------------------------------------------------------------------------

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hnswfns: no scalar fns".into()))
    }
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hnswfns: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        if handle != TABLE_HANDLE {
            return Err(types::Duckerror::Internal("unknown table handle".into()));
        }
        let mut it = args.into_iter();
        let index_name = match it.next() {
            Some(types::Duckvalue::Text(s)) => s.to_string(),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "hnsw_search: first argument must be the index name (VARCHAR)".into(),
                ))
            }
        };
        let query_json = match it.next() {
            Some(types::Duckvalue::Text(s)) => s.to_string(),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "hnsw_search: second argument must be a JSON float array (VARCHAR)".into(),
                ))
            }
        };
        let k: i64 = match it.next() {
            Some(types::Duckvalue::Int64(v)) => v,
            Some(types::Duckvalue::Int32(v)) => v as i64,
            Some(types::Duckvalue::Null) | None => 5,
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "hnsw_search: third argument k must be BIGINT".into(),
                ))
            }
        };
        if k < 0 {
            return Err(types::Duckerror::Invalidargument("hnsw_search: k must be >= 0".into()));
        }

        let query = parse_float_array(&query_json)?;
        let hits = search_named(&index_name, &query, k as u32)?;

        let rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = hits
            .into_iter()
            .map(|h| {
                vec![
                    types::Duckvalue::Int64(h.rowid),
                    types::Duckvalue::Float32(h.distance),
                ]
            })
            .collect();
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hnswfns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hnswfns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hnswfns: no casts".into()))
    }
}

/// Parse a JSON array of numbers into a Vec<f32>.
fn parse_float_array(s: &str) -> Result<std::vec::Vec<f32>, types::Duckerror> {
    let v: serde_json::Value = serde_json::from_str(s)
        .map_err(|e| types::Duckerror::Invalidargument(format!("hnsw_search: bad query JSON: {e}")))?;
    let arr = v.as_array().ok_or_else(|| {
        types::Duckerror::Invalidargument("hnsw_search: query must be a JSON array".into())
    })?;
    let mut out = std::vec::Vec::with_capacity(arr.len());
    for e in arr {
        let f = e.as_f64().ok_or_else(|| {
            types::Duckerror::Invalidargument("hnsw_search: query elements must be numbers".into())
        })?;
        out.push(f as f32);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn register_hnsw_search() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let args = vec![
        runtime::Funcarg {
            name: Some("index_name".into()),
            logical: types::Logicaltype::Text,
        },
        runtime::Funcarg {
            name: Some("query".into()),
            logical: types::Logicaltype::Text,
        },
        runtime::Funcarg {
            name: Some("k".into()),
            logical: types::Logicaltype::Int64,
        },
    ];
    let columns = vec![
        types::Columndef {
            name: "rowid".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "distance".into(),
            logical: types::Logicaltype::Float32,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Explicit kNN search over a wasm_hnsw index: hnsw_search(index_name, \
             '[..query..]', k) -> (rowid, distance)"
                .into(),
        ),
        tags: vec!["hnsw".into(), "vector".into(), "index".into()],
    };
    reg.register(
        "hnsw_search",
        &args,
        &columns,
        runtime::TableCallback::new(TABLE_HANDLE),
        Some(&opts),
    )?;
    Ok(())
}

export!(Extension);
