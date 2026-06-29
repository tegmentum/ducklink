//! Read ESRI .shp shapefiles as a DuckDB table function over an in-memory BLOB.
//!
//!   read_shp(data BLOB) -> table(shape_no BIGINT, shape_type VARCHAR, wkt VARCHAR)
//!
//! A real shapefile dataset is .shp + .shx + .dbf, but the geometry lives entirely
//! in the .shp stream. This reads just the .shp blob (no index/attributes) and emits
//! one row per shape: `shape_no` is 1-indexed, `shape_type` is the shape kind
//! (Point/Polyline/Polygon/Multipoint/...), and `wkt` is the geometry rendered as
//! WKT text (POINT, LINESTRING/MULTILINESTRING, POLYGON, MULTIPOINT). A null shape
//! yields NULL wkt. A malformed blob yields zero rows -- never a panic.
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

use shapefile::record::polygon::PolygonRing;
use shapefile::{Point, Shape};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_read_shp()?;
        Ok(types::Loadresult {
            name: "shapefile".into(),
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
    // major-4 columnar dispatch: shapefile is table-only, so the three columnar
    // hot methods are Unsupported stubs; call_table stays hand-written.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("shapefile: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // single registered table fn; any handle maps to read_shp
        let _ = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        let bytes: std::vec::Vec<u8> = match args.into_iter().next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            // accept TEXT too, so `read_shp(<varchar>)` degrades gracefully
            Some(types::Duckvalue::Text(s)) => s.into_bytes(),
            Some(types::Duckvalue::Null) | None => return Ok(Vec::new().into()),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "read_shp expects a single BLOB argument".into(),
                ))
            }
        };

        Ok(read(&bytes).into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("shapefile: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("shapefile: no casts".into()))
    }
}

export!(Extension);

/// Read the .shp bytes and emit one row per shape. Returns an empty result on any
/// malformed input rather than panicking or erroring.
fn read(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::new();

    // ShapeReader reads only the .shp stream (header + records); no .shx/.dbf needed.
    let mut reader = match shapefile::ShapeReader::new(Cursor::new(bytes.to_vec())) {
        Ok(r) => r,
        Err(_) => return out,
    };

    for (idx, res) in reader.iter_shapes().enumerate() {
        let shape = match res {
            Ok(s) => s,
            // Stop on the first malformed record; rows read so far are kept.
            Err(_) => break,
        };
        let shape_no = (idx as i64) + 1;
        let kind = shape_type_name(&shape);
        let wkt = shape_to_wkt(&shape);
        let wkt_val = match wkt {
            Some(s) => types::Duckvalue::Text(s.into()),
            None => types::Duckvalue::Null,
        };
        out.push(vec![
            types::Duckvalue::Int64(shape_no),
            types::Duckvalue::Text(kind.into()),
            wkt_val,
        ]);
    }

    out
}

/// Human-readable shape kind name, matching the shapefile crate's ShapeType naming.
fn shape_type_name(shape: &Shape) -> &'static str {
    match shape {
        Shape::NullShape => "NullShape",
        Shape::Point(_) => "Point",
        Shape::PointM(_) => "PointM",
        Shape::PointZ(_) => "PointZ",
        Shape::Polyline(_) => "Polyline",
        Shape::PolylineM(_) => "PolylineM",
        Shape::PolylineZ(_) => "PolylineZ",
        Shape::Polygon(_) => "Polygon",
        Shape::PolygonM(_) => "PolygonM",
        Shape::PolygonZ(_) => "PolygonZ",
        Shape::Multipoint(_) => "Multipoint",
        Shape::MultipointM(_) => "MultipointM",
        Shape::MultipointZ(_) => "MultipointZ",
        Shape::Multipatch(_) => "Multipatch",
    }
}

/// Render a shape as 2D WKT (x/y only). NullShape -> None (becomes NULL wkt).
fn shape_to_wkt(shape: &Shape) -> Option<std::string::String> {
    match shape {
        Shape::NullShape => None,

        Shape::Point(p) => Some(format!("POINT({})", coord(p.x, p.y))),
        Shape::PointM(p) => Some(format!("POINT({})", coord(p.x, p.y))),
        Shape::PointZ(p) => Some(format!("POINT({})", coord(p.x, p.y))),

        Shape::Polyline(pl) => Some(polyline_wkt(pl.parts())),
        Shape::PolylineM(pl) => Some(polyline_wkt_m(pl.parts())),
        Shape::PolylineZ(pl) => Some(polyline_wkt_z(pl.parts())),

        Shape::Polygon(pg) => Some(polygon_wkt(pg.rings().iter().map(ring_points))),
        Shape::PolygonM(pg) => Some(polygon_wkt(pg.rings().iter().map(ring_points_m))),
        Shape::PolygonZ(pg) => Some(polygon_wkt(pg.rings().iter().map(ring_points_z))),

        Shape::Multipoint(mp) => {
            Some(multipoint_wkt(mp.points().iter().map(|p| (p.x, p.y))))
        }
        Shape::MultipointM(mp) => {
            Some(multipoint_wkt(mp.points().iter().map(|p| (p.x, p.y))))
        }
        Shape::MultipointZ(mp) => {
            Some(multipoint_wkt(mp.points().iter().map(|p| (p.x, p.y))))
        }

        // Multipatch is a collection of PointZ parts; emit as MULTIPOLYGON-ish via
        // GEOMETRYCOLLECTION is overkill -- represent its rings like a polygon set.
        Shape::Multipatch(mp) => {
            let parts: std::vec::Vec<std::vec::Vec<(f64, f64)>> = mp
                .patches()
                .iter()
                .map(|patch| patch.points().iter().map(|p| (p.x, p.y)).collect())
                .collect();
            Some(parts_as_multilinestring(&parts))
        }
    }
}

// ---- WKT builders ---------------------------------------------------------

fn coord(x: f64, y: f64) -> std::string::String {
    format!("{} {}", fmt_f64(x), fmt_f64(y))
}

fn ring_points(ring: &PolygonRing<Point>) -> std::vec::Vec<(f64, f64)> {
    ring.points().iter().map(|p| (p.x, p.y)).collect()
}
fn ring_points_m(
    ring: &PolygonRing<shapefile::PointM>,
) -> std::vec::Vec<(f64, f64)> {
    ring.points().iter().map(|p| (p.x, p.y)).collect()
}
fn ring_points_z(
    ring: &PolygonRing<shapefile::PointZ>,
) -> std::vec::Vec<(f64, f64)> {
    ring.points().iter().map(|p| (p.x, p.y)).collect()
}

fn polyline_wkt(parts: &[std::vec::Vec<Point>]) -> std::string::String {
    let parts: std::vec::Vec<std::vec::Vec<(f64, f64)>> = parts
        .iter()
        .map(|part| part.iter().map(|p| (p.x, p.y)).collect())
        .collect();
    linestring_or_multi(&parts)
}
fn polyline_wkt_m(parts: &[std::vec::Vec<shapefile::PointM>]) -> std::string::String {
    let parts: std::vec::Vec<std::vec::Vec<(f64, f64)>> = parts
        .iter()
        .map(|part| part.iter().map(|p| (p.x, p.y)).collect())
        .collect();
    linestring_or_multi(&parts)
}
fn polyline_wkt_z(parts: &[std::vec::Vec<shapefile::PointZ>]) -> std::string::String {
    let parts: std::vec::Vec<std::vec::Vec<(f64, f64)>> = parts
        .iter()
        .map(|part| part.iter().map(|p| (p.x, p.y)).collect())
        .collect();
    linestring_or_multi(&parts)
}

/// One part -> LINESTRING, multiple parts -> MULTILINESTRING.
fn linestring_or_multi(parts: &[std::vec::Vec<(f64, f64)>]) -> std::string::String {
    if parts.len() == 1 {
        format!("LINESTRING({})", coord_list(&parts[0]))
    } else {
        parts_as_multilinestring(parts)
    }
}

fn parts_as_multilinestring(parts: &[std::vec::Vec<(f64, f64)>]) -> std::string::String {
    let inner: std::vec::Vec<std::string::String> = parts
        .iter()
        .map(|part| format!("({})", coord_list(part)))
        .collect();
    format!("MULTILINESTRING({})", inner.join(", "))
}

/// Build a POLYGON from rings (outer + holes). Each ring -> a parenthesised group.
fn polygon_wkt<I>(rings: I) -> std::string::String
where
    I: Iterator<Item = std::vec::Vec<(f64, f64)>>,
{
    let groups: std::vec::Vec<std::string::String> = rings
        .map(|ring| format!("({})", coord_list(&ring)))
        .collect();
    format!("POLYGON({})", groups.join(", "))
}

fn multipoint_wkt<I>(points: I) -> std::string::String
where
    I: Iterator<Item = (f64, f64)>,
{
    let inner: std::vec::Vec<std::string::String> =
        points.map(|(x, y)| coord(x, y)).collect();
    format!("MULTIPOINT({})", inner.join(", "))
}

fn coord_list(pts: &[(f64, f64)]) -> std::string::String {
    let parts: std::vec::Vec<std::string::String> =
        pts.iter().map(|(x, y)| coord(*x, *y)).collect();
    parts.join(", ")
}

/// Render an f64 without a trailing ".0" for whole numbers, so 1.0 -> "1".
fn fmt_f64(v: f64) -> std::string::String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

fn register_read_shp() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::ReadShp);

    let args = vec![runtime::Funcarg {
        name: Some("data".into()),
        logical: types::Logicaltype::Blob,
    }];
    let columns = vec![
        types::Columndef {
            name: "shape_no".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "shape_type".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "wkt".into(),
            logical: types::Logicaltype::Text,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Read ESRI .shp bytes into (shape_no, shape_type, wkt) rows, geometry as WKT".into(),
        ),
        tags: vec!["shapefile".into(), "shp".into(), "gis".into()],
    };
    reg.register("read_shp", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    ReadShp,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
