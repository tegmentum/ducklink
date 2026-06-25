//! Custom spatial R-tree index (Item 3 / deferred spatial keystone).
//!
//! This component proves the Item-3 custom-index capability is GENERAL: it reuses
//! the EXACT same `index` / `index-dispatch` WIT that hnswfns uses for HNSW, with
//! ZERO core change, to back a spatial R-tree instead. The only difference is the
//! interpretation of the `list<f32>` payload that the index WIT already carries:
//!
//!   * for HNSW (hnswfns) each `list<f32>` is a POINT vector (FLOAT[N]); search is
//!     kNN.
//!   * for the R-tree (this crate) each `list<f32>` is a BBOX of 4 floats
//!     (minx,miny,maxx,maxy); the indexed column is FLOAT[4]; search returns the
//!     rowids whose indexed bbox INTERSECTS the query bbox.
//!
//! Same FLOAT[N] ingest path: the core's WasmBoundIndex build pipeline feeds the
//! indexed column's rows to `index-append` as `vectors: list<list<f32>>`. We store
//! each as an rstar AABB tagged with its rowid, then `RTree::bulk_load` at build.
//!
//! Two faces over one shared, in-component index state (keyed by index NAME),
//! exactly like hnswfns:
//!   (a) the `index` / `index-dispatch` WIT surface the core drives for
//!       `CREATE INDEX ... USING wasm_rtree (bb)`, and
//!   (b) an explicit `rtree_search(index_name, bbox, limit) -> table(rowid)` table
//!       function over the SAME built R-tree. `bbox` is a JSON array of 4 floats,
//!       e.g. '[0,0,1.5,1.5]'.
//!
//! Also a helper scalar `bbox4(wkt) -> VARCHAR` that returns the bbox of a WKT
//! geometry as the JSON string '[minx,miny,maxx,maxy]'. (A FLOAT[4] return would
//! need a nested list/array duckvalue which the WIT does not support, so we return
//! VARCHAR JSON; the user casts: `CAST(bbox4(wkt) AS FLOAT[4])`.)
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-index" });

use duckdb::extension::{index, runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest, index_dispatch};

use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{RTree, AABB};

/// Opaque handle for the registered `rtree_search` table function.
const TABLE_HANDLE: u32 = 1;
/// Opaque handle for the registered `bbox4` scalar function.
const BBOX4_HANDLE: u32 = 2;

struct Extension;

// ---------------------------------------------------------------------------
// R-tree item: a 2-D axis-aligned bounding box tagged with its table rowid.
// rstar's GeomWithData wraps a geometry (here an AABB envelope) with arbitrary
// data; we tag each indexed bbox with its rowid (s64).
// ---------------------------------------------------------------------------

// Rectangle wraps an AABB and implements RTreeObject (so it can be indexed);
// GeomWithData tags it with the rowid.
type RectItem = GeomWithData<Rectangle<[f32; 2]>, i64>;

/// Build an rstar AABB from a [minx,miny,maxx,maxy] bbox.
fn aabb_from_bbox(b: &[f32; 4]) -> AABB<[f32; 2]> {
    // rstar normalizes corners, but pass lower/upper explicitly for clarity.
    let lower = [b[0].min(b[2]), b[1].min(b[3])];
    let upper = [b[0].max(b[2]), b[1].max(b[3])];
    AABB::from_corners(lower, upper)
}

/// Build an rstar Rectangle (an RTreeObject) from a bbox.
fn rect_from_bbox(b: &[f32; 4]) -> Rectangle<[f32; 2]> {
    Rectangle::from_aabb(aabb_from_bbox(b))
}

/// One index: either accumulating rows (pre-build) or a finalized R-tree.
struct IndexState {
    dims: usize,
    // Accumulation buffers (consumed at build).
    items: std::vec::Vec<RectItem>,
    // The finalized R-tree (None until index-build).
    tree: Option<RTree<RectItem>>,
}

impl IndexState {
    fn new(dims: usize) -> Self {
        IndexState {
            dims,
            items: std::vec::Vec::new(),
            tree: None,
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
// guest: load() registers the index type + the rtree_search table fn + bbox4.
// ---------------------------------------------------------------------------

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        index::register_index_type("wasm_rtree")?;
        register_rtree_search()?;
        register_bbox4()?;
        Ok(types::Loadresult {
            name: "rtreefns".into(),
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
// index-dispatch: build (create/append/build) + search + drop. The core drives
// create/append/build for CREATE INDEX. Each appended `vector` is a 4-float bbox
// rather than an N-float point -- the SAME `list<list<f32>>` ingest as HNSW.
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
                // Each indexed vector is a bbox: exactly 4 floats
                // (minx,miny,maxx,maxy). dims is 4 for an R-tree.
                if v.len() != 4 {
                    return Err(types::Duckerror::Invalidargument(format!(
                        "index_append: R-tree bbox must be 4 floats (minx,miny,maxx,maxy), got {}",
                        v.len()
                    )));
                }
                if st.dims != 4 {
                    return Err(types::Duckerror::Invalidargument(format!(
                        "index_append: wasm_rtree index dims must be 4 (FLOAT[4] bbox), got {}",
                        st.dims
                    )));
                }
                let bbox = [v[0], v[1], v[2], v[3]];
                st.items.push(GeomWithData::new(rect_from_bbox(&bbox), *rid));
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
            let items = std::mem::take(&mut st.items);
            // Bulk-load the R-tree from all appended (rect, rowid) items -- the
            // R-tree analogue of HNSW's Builder::build.
            st.tree = Some(RTree::bulk_load(items));
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

/// Run an intersection search against the built R-tree named `name`: return the
/// rowids whose indexed bbox INTERSECTS the query bbox `query` (4 floats). If
/// `k > 0`, cap the number of hits. Shared by index-search and rtree_search.
fn search_named(
    name: &str,
    query: &[f32],
    k: u32,
) -> Result<Vec<index_dispatch::IndexHit>, types::Duckerror> {
    if query.len() != 4 {
        return Err(types::Duckerror::Invalidargument(format!(
            "rtree search: query bbox must be 4 floats (minx,miny,maxx,maxy), got {}",
            query.len()
        )));
    }
    let qbox = [query[0], query[1], query[2], query[3]];
    let qaabb = aabb_from_bbox(&qbox);
    INDEXES.with(|m| {
        let m = m.borrow();
        let st = m
            .get(name)
            .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown index '{name}'")))?;
        let tree = st.tree.as_ref().ok_or_else(|| {
            types::Duckerror::Invalidstate(format!("index '{name}' has not been built"))
        })?;
        let mut hits: std::vec::Vec<index_dispatch::IndexHit> = std::vec::Vec::new();
        for item in tree.locate_in_envelope_intersecting(&qaabb) {
            if k > 0 && hits.len() >= k as usize {
                break;
            }
            // distance is unused for an R-tree intersection result; report 0.0.
            hits.push(index_dispatch::IndexHit {
                rowid: item.data,
                distance: 0.0,
            });
        }
        Ok(hits.into())
    })
}

// ---------------------------------------------------------------------------
// callback-dispatch: the rtree_search table function + the bbox4 scalar.
// ---------------------------------------------------------------------------

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
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
            )?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        if handle != BBOX4_HANDLE {
            return Err(types::Duckerror::Internal("unknown scalar handle".into()));
        }
        // bbox4(wkt) -> VARCHAR '[minx,miny,maxx,maxy]' (NULL on parse failure).
        let wkt = match args.into_iter().next() {
            Some(types::Duckvalue::Text(s)) => s.to_string(),
            _ => return Ok(types::Duckvalue::Null),
        };
        match bbox4_of_wkt(&wkt) {
            Some(json) => Ok(types::Duckvalue::Text(json.into())),
            None => Ok(types::Duckvalue::Null),
        }
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
                    "rtree_search: first argument must be the index name (VARCHAR)".into(),
                ))
            }
        };
        let bbox_json = match it.next() {
            Some(types::Duckvalue::Text(s)) => s.to_string(),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "rtree_search: second argument must be a JSON bbox array (VARCHAR)".into(),
                ))
            }
        };
        let limit: i64 = match it.next() {
            Some(types::Duckvalue::Int64(v)) => v,
            Some(types::Duckvalue::Int32(v)) => v as i64,
            Some(types::Duckvalue::Null) | None => 0,
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "rtree_search: third argument limit must be BIGINT".into(),
                ))
            }
        };
        if limit < 0 {
            return Err(types::Duckerror::Invalidargument(
                "rtree_search: limit must be >= 0".into(),
            ));
        }

        let query = parse_float_array(&bbox_json)?;
        let hits = search_named(&index_name, &query, limit as u32)?;

        let rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = hits
            .into_iter()
            .map(|h| vec![types::Duckvalue::Int64(h.rowid)])
            .collect();
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rtreefns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rtreefns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("rtreefns: no casts".into()))
    }
}

/// Parse a JSON array of numbers into a Vec<f32>.
fn parse_float_array(s: &str) -> Result<std::vec::Vec<f32>, types::Duckerror> {
    let v: serde_json::Value = serde_json::from_str(s).map_err(|e| {
        types::Duckerror::Invalidargument(format!("rtree_search: bad bbox JSON: {e}"))
    })?;
    let arr = v.as_array().ok_or_else(|| {
        types::Duckerror::Invalidargument("rtree_search: bbox must be a JSON array".into())
    })?;
    let mut out = std::vec::Vec::with_capacity(arr.len());
    for e in arr {
        let f = e.as_f64().ok_or_else(|| {
            types::Duckerror::Invalidargument("rtree_search: bbox elements must be numbers".into())
        })?;
        out.push(f as f32);
    }
    Ok(out)
}

/// Compute the bounding box of a WKT geometry as JSON '[minx,miny,maxx,maxy]'.
/// Returns None on any parse failure or for empty geometries (no bounding rect).
fn bbox4_of_wkt(s: &str) -> Option<std::string::String> {
    use geo::algorithm::bounding_rect::BoundingRect;
    use geo::geometry::Geometry;
    use std::str::FromStr;
    use wkt::Wkt;
    let w: Wkt<f64> = Wkt::from_str(s).ok()?;
    let g = Geometry::<f64>::try_from(w).ok()?;
    let rect = g.bounding_rect()?;
    let min = rect.min();
    let max = rect.max();
    Some(format!("[{},{},{},{}]", min.x, min.y, max.x, max.y).into())
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn register_rtree_search() -> Result<(), types::Duckerror> {
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
            name: Some("bbox".into()),
            logical: types::Logicaltype::Text,
        },
        runtime::Funcarg {
            name: Some("limit".into()),
            logical: types::Logicaltype::Int64,
        },
    ];
    let columns = vec![types::Columndef {
        name: "rowid".into(),
        logical: types::Logicaltype::Int64,
    }];
    let opts = runtime::Extopts {
        description: Some(
            "Spatial intersection search over a wasm_rtree index: \
             rtree_search(index_name, '[minx,miny,maxx,maxy]', limit) -> (rowid); \
             returns rowids whose indexed bbox intersects the query bbox (limit=0 \
             = all)."
                .into(),
        ),
        tags: vec!["rtree".into(), "spatial".into(), "index".into()],
    };
    reg.register(
        "rtree_search",
        &args,
        &columns,
        runtime::TableCallback::new(TABLE_HANDLE),
        Some(&opts),
    )?;
    Ok(())
}

fn register_bbox4() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let args = vec![runtime::Funcarg {
        name: Some("wkt".into()),
        logical: types::Logicaltype::Text,
    }];
    let opts = runtime::Funcopts {
        description: Some(
            "bbox4(wkt) -> VARCHAR '[minx,miny,maxx,maxy]': bounding box of a WKT \
             geometry as JSON; CAST(bbox4(wkt) AS FLOAT[4]) to feed a wasm_rtree \
             index column."
                .into(),
        ),
        tags: vec!["rtree".into(), "spatial".into(), "geo".into()],
        attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
    };
    reg.register(
        "bbox4",
        &args,
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(BBOX4_HANDLE),
        Some(&opts),
    )?;
    Ok(())
}

export!(Extension);
