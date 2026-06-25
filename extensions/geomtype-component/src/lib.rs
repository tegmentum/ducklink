//! A COHERENT custom logical type: `geometry`.
//!
//! Item 5 (scoped) proof. The component registers, mid-session on LOAD:
//!   * a logical type `geometry` aliased to BLOB (the physical carries WKB bytes),
//!   * two casts:  VARCHAR -> geometry   (parse WKT text  -> WKB blob)
//!                 geometry -> VARCHAR   (decode WKB blob -> WKT text),
//!   * two scalars over the native `geometry` type (physical BLOB / WKB):
//!                 geom_area(geometry) -> DOUBLE
//!                 geom_astext(geometry) -> VARCHAR
//!
//! So the type round-trips through SQL with its own text representation, and
//! functions consume the typed value (not just casts). Pure Rust: `geo`
//! (algorithms) + `wkt` (text I/O) + a small self-contained WKB codec.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{catalog, runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use geo::algorithm::area::Area;
use geo::geometry::Geometry;
use std::str::FromStr;
use wkt::{ToWkt, Wkt};

struct Extension;

// =====================================================================
// WKB codec (self-contained, little-endian, ISO/OGC standard WKB).
//   byte order: 1 (LE)
//   type code:  1=Point 2=LineString 3=Polygon 4=MultiPoint
//               5=MultiLineString 6=MultiPolygon 7=GeometryCollection
// 2D doubles only (no Z/M, no SRID) — sufficient for the geometry proof.
// =====================================================================
#[path = "wkb.rs"]
mod wkb;

// ---------- pure helpers ----------

fn parse_wkt(s: &str) -> Option<Geometry<f64>> {
    let w = Wkt::<f64>::from_str(s).ok()?;
    Geometry::<f64>::try_from(w).ok()
}

fn geom_to_wkt(g: &Geometry<f64>) -> String {
    g.wkt_string().into()
}

fn total_area(g: &Geometry<f64>) -> f64 {
    match g {
        Geometry::Polygon(p) => p.unsigned_area(),
        Geometry::MultiPolygon(mp) => mp.unsigned_area(),
        Geometry::Rect(r) => r.unsigned_area(),
        Geometry::Triangle(t) => t.unsigned_area(),
        _ => 0.0,
    }
}

// ---------- guest impl ----------

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_type_and_casts()?;
        register_scalars()?;
        Ok(types::Loadresult {
            name: "geomtype".into(),
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
        let which = scalar_handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let n = types::Duckvalue::Null;
        // The `geometry`-typed argument arrives physically as a BLOB (WKB bytes).
        let geom = match args.first() {
            Some(types::Duckvalue::Blob(b)) => wkb::decode(b),
            _ => None,
        };
        Ok(match (which, geom) {
            (Scalar::Area, Some(g)) => types::Duckvalue::Float64(total_area(&g)),
            (Scalar::AsText, Some(g)) => types::Duckvalue::Text(geom_to_wkt(&g)),
            _ => n,
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("geomtype: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("geomtype: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("geomtype: no pragmas".into()))
    }

    /// The cast dispatch. The core calls this with the SOURCE physical value and
    /// expects the TARGET physical value. We key on the value shape:
    ///   * Text(wkt)  ->  Blob(wkb)   (VARCHAR -> geometry)
    ///   * Blob(wkb)  ->  Text(wkt)   (geometry -> VARCHAR)
    fn call_cast(
        handle: u32,
        value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let dir = cast_handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown cast handle".into()))?;
        Ok(match (dir, value) {
            (CastDir::TextToGeom, types::Duckvalue::Text(s)) => match parse_wkt(&s) {
                Some(g) => match wkb::encode(&g) {
                    Some(b) => types::Duckvalue::Blob(b.into()),
                    None => return Err(types::Duckerror::Invalidargument(
                        "could not encode geometry to WKB".into(),
                    )),
                },
                None => {
                    return Err(types::Duckerror::Invalidargument(
                        std::format!("invalid WKT geometry: {s}").into(),
                    ))
                }
            },
            (CastDir::GeomToText, types::Duckvalue::Blob(b)) => match wkb::decode(&b) {
                Some(g) => types::Duckvalue::Text(geom_to_wkt(&g)),
                None => {
                    return Err(types::Duckerror::Invalidargument(
                        "invalid WKB geometry blob".into(),
                    ))
                }
            },
            // Builtin geometry (delivered as a Blob thanks to the core's
            // GEOMETRY -> Blob arm) -> geom2: decode + re-encode the WKB.
            (CastDir::GeomToGeom, types::Duckvalue::Blob(b)) => match wkb::decode(&b) {
                Some(g) => match wkb::encode(&g) {
                    Some(out) => types::Duckvalue::Blob(out.into()),
                    None => {
                        return Err(types::Duckerror::Invalidargument(
                            "could not re-encode geometry WKB".into(),
                        ))
                    }
                },
                None => {
                    return Err(types::Duckerror::Invalidargument(
                        "invalid builtin-geometry WKB blob".into(),
                    ))
                }
            },
            // NULL passes through unchanged.
            (_, types::Duckvalue::Null) => types::Duckvalue::Null,
            (_, other) => {
                return Err(types::Duckerror::Invalidargument(
                    std::format!("geomtype cast: unexpected value {other:?}").into(),
                ))
            }
        })
    }
}

export!(Extension);

// ---------- registration ----------

fn register_type_and_casts() -> Result<(), types::Duckerror> {
    // 1. The custom logical type: geom2, physically a BLOB carrying WKB.
    //    (`geometry` is a reserved BUILTIN type in DuckDB v1.5.x — a native WKB
    //    GEOMETRY with its own physical id 40 — so we register a fresh name to
    //    prove the NEW custom-type + custom-cast surface, not the builtin.)
    catalog::register_logical_type(&catalog::LogicalType {
        name: TYPE_NAME.into(),
        physical: "BLOB".into(),
    })
    .map_err(|e| types::Duckerror::Internal(std::format!("register {TYPE_NAME} type: {e}").into()))?;

    // 2a. VARCHAR -> geom2  (assignment cast so INSERT '...'::geom2 and implicit
    //     assignment on INSERT both fire). WKT text -> WKB blob.
    let h_to = next_handle();
    cast_handlers()
        .lock()
        .unwrap()
        .insert(h_to, CastDir::TextToGeom);
    catalog::register_cast(
        &catalog::CastSpec {
            from: "VARCHAR".into(),
            to: TYPE_NAME.into(),
            kind: catalog::CastKind::Assignment,
        },
        runtime::CastCallback::new(h_to),
    )
    .map_err(|e| {
        types::Duckerror::Internal(std::format!("register VARCHAR->{TYPE_NAME}: {e}").into())
    })?;

    // 2b. geom2 -> VARCHAR  (explicit cast for SELECT shape::VARCHAR). WKB -> WKT.
    let h_from = next_handle();
    cast_handlers()
        .lock()
        .unwrap()
        .insert(h_from, CastDir::GeomToText);
    catalog::register_cast(
        &catalog::CastSpec {
            from: TYPE_NAME.into(),
            to: "VARCHAR".into(),
            kind: catalog::CastKind::Explicit,
        },
        runtime::CastCallback::new(h_from),
    )
    .map_err(|e| {
        types::Duckerror::Internal(std::format!("register {TYPE_NAME}->VARCHAR: {e}").into())
    })?;

    // 2c. The BUILTIN `geometry` -> geom2 cast. This proves the core's
    //     DUCKDB_TYPE_GEOMETRY -> Blob arm: a builtin-`geometry` value (WKB
    //     string_t blob, physical id 40) is delivered to our cast callback AS A
    //     BLOB, which we re-encode into geom2's identical WKB. So a builtin
    //     geometry value becomes readable by a component cast. Registration is
    //     best-effort (skip if the builtin type is unavailable in the lean core).
    let h_builtin = next_handle();
    cast_handlers()
        .lock()
        .unwrap()
        .insert(h_builtin, CastDir::GeomToGeom);
    let _ = catalog::register_cast(
        &catalog::CastSpec {
            from: "geometry".into(),
            to: TYPE_NAME.into(),
            kind: catalog::CastKind::Explicit,
        },
        runtime::CastCallback::new(h_builtin),
    );

    Ok(())
}

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    // The argument type is the custom `geometry` type, physically a BLOB. The
    // core resolves the function arg's logical type from the registered name; we
    // advertise the physical (BLOB) here since that is what flows in the vector.
    let geom = types::Logicaltype::Blob;
    let dbl = types::Logicaltype::Float64;
    let txt = types::Logicaltype::Text;

    let reg_one = |name: &str,
                       f: Scalar,
                       ret: types::Logicaltype,
                       desc: &str|
     -> Result<(), types::Duckerror> {
        let h = next_handle();
        scalar_handlers().lock().unwrap().insert(h, f);
        let argv = vec![runtime::Funcarg {
            name: Some("geom".into()),
            logical: geom,
        }];
        reg.register(
            name,
            &argv,
            ret,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["geometry".into(), "geo".into()],
                attributes: det,
            }),
        )?;
        Ok(())
    };

    reg_one("geom_area", Scalar::Area, dbl, "area of a geometry (WKB)")?;
    reg_one(
        "geom_astext",
        Scalar::AsText,
        txt,
        "geometry (WKB) -> WKT text",
    )?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Scalar {
    Area,
    AsText,
}

#[derive(Clone, Copy, PartialEq)]
enum CastDir {
    TextToGeom,
    GeomToText,
    // Builtin `geometry` (WKB blob, physical id 40) -> geom2 (WKB blob). Proves
    // the core's GEOMETRY -> Blob arm: the value arrives as a Blob and we decode
    // + re-encode it, confirming a geometry value is readable as a blob.
    GeomToGeom,
}

/// The custom logical-type name. `geometry` is a builtin in DuckDB v1.5.x, so we
/// use a fresh name to demonstrate registering a genuinely-new named type.
const TYPE_NAME: &str = "geom2";

static NEXT: AtomicU32 = AtomicU32::new(1);
fn next_handle() -> u32 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

static SCALARS: OnceLock<Mutex<HashMap<u32, Scalar>>> = OnceLock::new();
fn scalar_handlers() -> &'static Mutex<HashMap<u32, Scalar>> {
    SCALARS.get_or_init(|| Mutex::new(HashMap::new()))
}

static CASTS: OnceLock<Mutex<HashMap<u32, CastDir>>> = OnceLock::new();
fn cast_handlers() -> &'static Mutex<HashMap<u32, CastDir>> {
    CASTS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------- native unit tests (pure codec, no wit) ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkt_wkb_roundtrip_polygon() {
        let g = parse_wkt("POLYGON((0 0,0 2,2 2,2 0,0 0))").unwrap();
        let wkb = wkb::encode(&g).unwrap();
        let back = wkb::decode(&wkb).unwrap();
        assert_eq!(geom_to_wkt(&back), "POLYGON((0 0,0 2,2 2,2 0,0 0))");
        assert_eq!(total_area(&back), 4.0);
    }

    #[test]
    fn wkt_wkb_roundtrip_point_line() {
        for w in ["POINT(1 2)", "LINESTRING(0 0,3 4)", "MULTIPOINT(0 0,1 1)"] {
            let g = parse_wkt(w).unwrap();
            let b = wkb::encode(&g).unwrap();
            let back = wkb::decode(&b).unwrap();
            assert_eq!(parse_wkt(&geom_to_wkt(&back)).is_some(), true);
        }
    }

    #[test]
    fn invalid_wkt_is_none() {
        assert!(parse_wkt("NOT A GEOM").is_none());
    }

    // ---- fuzz regressions (cargo-fuzz; fuzz/fuzz_targets/wkb_decode.rs) ------

    /// A WKB count field is a u32 (up to ~4.2 billion). Pre-fix, the decoder did
    /// `Vec::with_capacity(n)`; on the wasm32 deployment target that reservation
    /// either capacity-overflow-panics (n*elem_size > isize::MAX) or OOMs the 4
    /// GiB linear memory before a single coordinate is read. The MAX_PREALLOC cap
    /// bounds the initial reservation; the bytes still run out, so we get None.
    #[test]
    fn wkb_absurd_count_does_not_oom() {
        // LE(1) + type 2 (LineString) + count 0xFFFFFFFF, then nothing.
        let bytes = [1u8, 2, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(wkb::decode(&bytes).is_none());
        // type 7 (GeometryCollection) with a huge sub-count, no subgeoms.
        let gc = [1u8, 7, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(wkb::decode(&gc).is_none());
    }

    /// A maliciously deep nest of GeometryCollections must hit the depth limit
    /// and return None, never recurse until the native stack overflows.
    #[test]
    fn wkb_deep_nesting_does_not_stack_overflow() {
        // Each level: LE(1), type 7, count 1. 1000 levels >> MAX_DEPTH (64).
        let mut bytes = Vec::new();
        for _ in 0..1000 {
            bytes.push(1u8); // LE
            bytes.extend_from_slice(&7u32.to_le_bytes()); // GeometryCollection
            bytes.extend_from_slice(&1u32.to_le_bytes()); // 1 sub-geometry
        }
        assert!(wkb::decode(&bytes).is_none());
    }

    /// Foreign byte order, unknown type codes, truncated and empty inputs are all
    /// graceful (None), never a panic.
    #[test]
    fn wkb_malformed_inputs_are_none() {
        assert!(wkb::decode(&[]).is_none()); // empty
        assert!(wkb::decode(&[0u8]).is_none()); // big-endian order byte rejected
        assert!(wkb::decode(&[1u8, 99, 0, 0, 0]).is_none()); // unknown type 99
        assert!(wkb::decode(&[1u8, 1, 0, 0, 0, 0, 0]).is_none()); // truncated Point
    }
}
