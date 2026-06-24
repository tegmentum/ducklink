//! A useful SUBSET of DuckDB's `spatial` extension ST_* functions, pure Rust.
//! Geometry is represented as WKT text (VARCHAR in/out). Parse errors -> NULL,
//! never panic. Built on the `geo` (algorithms) + `wkt` (parse/format) crates.
//!
//! Functions: ST_Point, ST_GeomFromText, ST_AsText, ST_X, ST_Y, ST_Distance,
//! ST_Area, ST_Length, ST_Centroid, ST_Contains, ST_Intersects, ST_Within,
//! ST_Envelope, ST_AsGeoJSON.
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

use geo::algorithm::{
    bounding_rect::BoundingRect, centroid::Centroid, contains::Contains,
    euclidean_distance::EuclideanDistance, euclidean_length::EuclideanLength, area::Area,
    intersects::Intersects,
};
use geo::geometry::Geometry;
use std::str::FromStr;
use wkt::{ToWkt, Wkt};

struct Extension;

// ---------- geometry helpers (pure, no panics) ----------

/// Parse WKT text into a geo Geometry. Returns None on any parse failure.
fn parse_geom(s: &str) -> Option<Geometry<f64>> {
    let w = Wkt::<f64>::from_str(s).ok()?;
    Geometry::<f64>::try_from(w).ok()
}

/// Format a geo Geometry back to canonical WKT text.
fn geom_to_wkt(g: &Geometry<f64>) -> String {
    g.wkt_string().into()
}

/// Total length: perimeter for polygons, line length for lines, 0 for points.
fn total_length(g: &Geometry<f64>) -> f64 {
    match g {
        Geometry::Line(l) => l.euclidean_length(),
        Geometry::LineString(ls) => ls.euclidean_length(),
        Geometry::MultiLineString(mls) => mls.euclidean_length(),
        Geometry::Polygon(p) => {
            p.exterior().euclidean_length()
                + p.interiors().iter().map(|r| r.euclidean_length()).sum::<f64>()
        }
        Geometry::MultiPolygon(mp) => mp
            .iter()
            .map(|p| {
                p.exterior().euclidean_length()
                    + p.interiors().iter().map(|r| r.euclidean_length()).sum::<f64>()
            })
            .sum(),
        _ => 0.0,
    }
}

/// Unsigned area (0 for non-areal geometries).
fn total_area(g: &Geometry<f64>) -> f64 {
    match g {
        Geometry::Polygon(p) => p.unsigned_area(),
        Geometry::MultiPolygon(mp) => mp.unsigned_area(),
        Geometry::Rect(r) => r.unsigned_area(),
        Geometry::Triangle(t) => t.unsigned_area(),
        _ => 0.0,
    }
}

/// Bounding box as a polygon WKT (envelope).
fn envelope_wkt(g: &Geometry<f64>) -> Option<String> {
    let rect = g.bounding_rect()?;
    let poly = rect.to_polygon();
    Some(Geometry::Polygon(poly).wkt_string().into())
}

/// First point coordinate of a point geometry.
fn point_xy(g: &Geometry<f64>) -> Option<(f64, f64)> {
    match g {
        Geometry::Point(p) => Some((p.x(), p.y())),
        _ => None,
    }
}

fn geojson_string(g: &Geometry<f64>) -> Option<String> {
    let gj = geojson::Geometry::try_from(g).ok()?;
    Some(gj.to_string().into())
}

// ---------- arg extraction ----------

fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        Some(types::Duckvalue::Uint64(v)) => Some(*v as f64),
        _ => None,
    }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn geom_arg(args: &[types::Duckvalue], i: usize) -> Option<Geometry<f64>> {
    parse_geom(&text_arg(args, i)?)
}

// ---------- guest impl ----------

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "spatialfns".into(),
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
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let n = types::Duckvalue::Null;
        Ok(match which {
            F::Point => match (f64_arg(&args, 0), f64_arg(&args, 1)) {
                (Some(x), Some(y)) => {
                    let g: Geometry<f64> = geo::Point::new(x, y).into();
                    types::Duckvalue::Text(geom_to_wkt(&g))
                }
                _ => n,
            },
            F::GeomFromText | F::AsText => match geom_arg(&args, 0) {
                Some(g) => types::Duckvalue::Text(geom_to_wkt(&g)),
                None => n,
            },
            F::X => match geom_arg(&args, 0).and_then(|g| point_xy(&g)) {
                Some((x, _)) => types::Duckvalue::Float64(x),
                None => n,
            },
            F::Y => match geom_arg(&args, 0).and_then(|g| point_xy(&g)) {
                Some((_, y)) => types::Duckvalue::Float64(y),
                None => n,
            },
            F::Distance => match (geom_arg(&args, 0), geom_arg(&args, 1)) {
                (Some(a), Some(b)) => types::Duckvalue::Float64(a.euclidean_distance(&b)),
                _ => n,
            },
            F::Area => match geom_arg(&args, 0) {
                Some(g) => types::Duckvalue::Float64(total_area(&g)),
                None => n,
            },
            F::Length => match geom_arg(&args, 0) {
                Some(g) => types::Duckvalue::Float64(total_length(&g)),
                None => n,
            },
            F::Centroid => match geom_arg(&args, 0).and_then(|g| g.centroid()) {
                Some(p) => types::Duckvalue::Text(geom_to_wkt(&Geometry::Point(p))),
                None => n,
            },
            F::Contains => match (geom_arg(&args, 0), geom_arg(&args, 1)) {
                (Some(a), Some(b)) => types::Duckvalue::Boolean(a.contains(&b)),
                _ => n,
            },
            F::Within => match (geom_arg(&args, 0), geom_arg(&args, 1)) {
                (Some(a), Some(b)) => types::Duckvalue::Boolean(b.contains(&a)),
                _ => n,
            },
            F::Intersects => match (geom_arg(&args, 0), geom_arg(&args, 1)) {
                (Some(a), Some(b)) => types::Duckvalue::Boolean(a.intersects(&b)),
                _ => n,
            },
            F::Envelope => match geom_arg(&args, 0).and_then(|g| envelope_wkt(&g)) {
                Some(s) => types::Duckvalue::Text(s),
                None => n,
            },
            F::AsGeoJSON => match geom_arg(&args, 0).and_then(|g| geojson_string(&g)) {
                Some(s) => types::Duckvalue::Text(s),
                None => n,
            },
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("spatialfns: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("spatialfns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("spatialfns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("spatialfns: no casts".into()))
    }
}

export!(Extension);

// ---------- registration ----------

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let tags = || vec!["spatial".into(), "geo".into()];
    let txt = types::Logicaltype::Text;
    let dbl = types::Logicaltype::Float64;
    let boolean = types::Logicaltype::Boolean;

    let mut reg_one = |name: &str,
                       f: F,
                       sig: &[(&str, types::Logicaltype)],
                       ret: types::Logicaltype,
                       desc: &str|
     -> Result<(), types::Duckerror> {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        let argv: Vec<runtime::Funcarg> = sig
            .iter()
            .map(|(n, lt)| runtime::Funcarg {
                name: Some((*n).into()),
                logical: *lt,
            })
            .collect();
        reg.register(
            name,
            &argv,
            ret,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: tags(),
                attributes: det,
            }),
        )?;
        Ok(())
    };

    reg_one(
        "ST_Point",
        F::Point,
        &[("x", dbl), ("y", dbl)],
        txt,
        "x,y -> POINT WKT",
    )?;
    reg_one(
        "ST_GeomFromText",
        F::GeomFromText,
        &[("wkt", txt)],
        txt,
        "validate/normalize WKT",
    )?;
    reg_one("ST_AsText", F::AsText, &[("geom", txt)], txt, "geometry -> WKT")?;
    reg_one("ST_X", F::X, &[("geom", txt)], dbl, "point X coordinate")?;
    reg_one("ST_Y", F::Y, &[("geom", txt)], dbl, "point Y coordinate")?;
    reg_one(
        "ST_Distance",
        F::Distance,
        &[("a", txt), ("b", txt)],
        dbl,
        "euclidean distance",
    )?;
    reg_one("ST_Area", F::Area, &[("geom", txt)], dbl, "area")?;
    reg_one(
        "ST_Length",
        F::Length,
        &[("geom", txt)],
        dbl,
        "length/perimeter",
    )?;
    reg_one(
        "ST_Centroid",
        F::Centroid,
        &[("geom", txt)],
        txt,
        "centroid POINT WKT",
    )?;
    reg_one(
        "ST_Contains",
        F::Contains,
        &[("a", txt), ("b", txt)],
        boolean,
        "a contains b",
    )?;
    reg_one(
        "ST_Within",
        F::Within,
        &[("a", txt), ("b", txt)],
        boolean,
        "a within b",
    )?;
    reg_one(
        "ST_Intersects",
        F::Intersects,
        &[("a", txt), ("b", txt)],
        boolean,
        "a intersects b",
    )?;
    reg_one(
        "ST_Envelope",
        F::Envelope,
        &[("geom", txt)],
        txt,
        "bounding box POLYGON WKT",
    )?;
    reg_one(
        "ST_AsGeoJSON",
        F::AsGeoJSON,
        &[("geom", txt)],
        txt,
        "geometry -> GeoJSON",
    )?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum F {
    Point,
    GeomFromText,
    AsText,
    X,
    Y,
    Distance,
    Area,
    Length,
    Centroid,
    Contains,
    Within,
    Intersects,
    Envelope,
    AsGeoJSON,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------- native unit tests (pure geometry helpers, no wit) ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn rnd(v: f64) -> f64 {
        (v * 1e6).round() / 1e6
    }

    #[test]
    fn point_roundtrip() {
        let g: Geometry<f64> = geo::Point::new(1.0, 2.0).into();
        assert_eq!(geom_to_wkt(&g), "POINT(1 2)");
        let p = parse_geom("POINT(1 2)").unwrap();
        assert_eq!(point_xy(&p), Some((1.0, 2.0)));
    }

    #[test]
    fn geomfromtext_invalid_is_none() {
        assert!(parse_geom("NOT A GEOM").is_none());
        assert!(parse_geom("POINT(1)").is_none());
    }

    #[test]
    fn xy() {
        let g = parse_geom("POINT(3 7)").unwrap();
        assert_eq!(point_xy(&g), Some((3.0, 7.0)));
        // non-point -> None
        assert!(point_xy(&parse_geom("LINESTRING(0 0,1 1)").unwrap()).is_none());
    }

    #[test]
    fn distance() {
        let a = parse_geom("POINT(0 0)").unwrap();
        let b = parse_geom("POINT(3 4)").unwrap();
        assert_eq!(a.euclidean_distance(&b), 5.0);
    }

    #[test]
    fn area() {
        let g = parse_geom("POLYGON((0 0,0 2,2 2,2 0,0 0))").unwrap();
        assert_eq!(total_area(&g), 4.0);
        // a line has no area
        assert_eq!(total_length(&parse_geom("POINT(0 0)").unwrap()), 0.0);
    }

    #[test]
    fn length() {
        let g = parse_geom("LINESTRING(0 0,3 4)").unwrap();
        assert_eq!(total_length(&g), 5.0);
        // polygon perimeter
        let p = parse_geom("POLYGON((0 0,0 2,2 2,2 0,0 0))").unwrap();
        assert_eq!(total_length(&p), 8.0);
    }

    #[test]
    fn centroid() {
        let g = parse_geom("POLYGON((0 0,0 2,2 2,2 0,0 0))").unwrap();
        let c = g.centroid().unwrap();
        assert_eq!((rnd(c.x()), rnd(c.y())), (1.0, 1.0));
    }

    #[test]
    fn contains_within() {
        let poly = parse_geom("POLYGON((0 0,0 4,4 4,4 0,0 0))").unwrap();
        let pt = parse_geom("POINT(1 1)").unwrap();
        assert!(poly.contains(&pt));
        assert!(!pt.contains(&poly));
        // within is contains with args swapped
        assert!(poly.contains(&pt)); // pt within poly
    }

    #[test]
    fn intersects() {
        let a = parse_geom("LINESTRING(0 0,4 4)").unwrap();
        let b = parse_geom("LINESTRING(0 4,4 0)").unwrap();
        assert!(a.intersects(&b));
        let c = parse_geom("POINT(10 10)").unwrap();
        assert!(!a.intersects(&c));
    }

    #[test]
    fn envelope() {
        let g = parse_geom("LINESTRING(0 0,3 4)").unwrap();
        let env = envelope_wkt(&g).unwrap();
        // bounding box of (0,0)-(3,4)
        assert!(env.starts_with("POLYGON"));
        assert!(env.contains("3 4") || env.contains("0 0"));
    }

    #[test]
    fn geojson() {
        let g = parse_geom("POINT(1 2)").unwrap();
        let gj = geojson_string(&g).unwrap();
        assert!(gj.contains("Point"));
        assert!(gj.contains("[1.0,2.0]") || gj.contains("[1,2]"));
    }
}
