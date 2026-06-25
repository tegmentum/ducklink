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
mod wkb {
    use super::*;
    use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
    use geo::geometry::{
        GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
    };
    use std::io::{Cursor, Read, Write};

    const LE: u8 = 1;

    pub fn encode(g: &Geometry<f64>) -> Option<std::vec::Vec<u8>> {
        let mut buf: std::vec::Vec<u8> = std::vec::Vec::new();
        write_geom(&mut buf, g).ok()?;
        Some(buf)
    }

    pub fn decode(bytes: &[u8]) -> Option<Geometry<f64>> {
        let mut c = Cursor::new(bytes);
        read_geom(&mut c).ok()
    }

    fn write_ring<W: Write>(w: &mut W, ls: &LineString<f64>) -> std::io::Result<()> {
        w.write_u32::<LittleEndian>(ls.0.len() as u32)?;
        for c in &ls.0 {
            w.write_f64::<LittleEndian>(c.x)?;
            w.write_f64::<LittleEndian>(c.y)?;
        }
        Ok(())
    }

    fn write_geom<W: Write>(w: &mut W, g: &Geometry<f64>) -> std::io::Result<()> {
        match g {
            Geometry::Point(p) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(1)?;
                w.write_f64::<LittleEndian>(p.x())?;
                w.write_f64::<LittleEndian>(p.y())?;
            }
            Geometry::LineString(ls) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(2)?;
                write_ring(w, ls)?;
            }
            Geometry::Polygon(poly) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(3)?;
                let n = 1 + poly.interiors().len();
                w.write_u32::<LittleEndian>(n as u32)?;
                write_ring(w, poly.exterior())?;
                for r in poly.interiors() {
                    write_ring(w, r)?;
                }
            }
            Geometry::MultiPoint(mp) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(4)?;
                w.write_u32::<LittleEndian>(mp.0.len() as u32)?;
                for p in &mp.0 {
                    write_geom(w, &Geometry::Point(*p))?;
                }
            }
            Geometry::MultiLineString(mls) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(5)?;
                w.write_u32::<LittleEndian>(mls.0.len() as u32)?;
                for ls in &mls.0 {
                    write_geom(w, &Geometry::LineString(ls.clone()))?;
                }
            }
            Geometry::MultiPolygon(mp) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(6)?;
                w.write_u32::<LittleEndian>(mp.0.len() as u32)?;
                for poly in &mp.0 {
                    write_geom(w, &Geometry::Polygon(poly.clone()))?;
                }
            }
            Geometry::GeometryCollection(gc) => {
                w.write_u8(LE)?;
                w.write_u32::<LittleEndian>(7)?;
                w.write_u32::<LittleEndian>(gc.0.len() as u32)?;
                for sub in &gc.0 {
                    write_geom(w, sub)?;
                }
            }
            // Rect / Triangle / Line normalize to their polygon/linestring forms.
            Geometry::Rect(r) => write_geom(w, &Geometry::Polygon(r.to_polygon()))?,
            Geometry::Triangle(t) => write_geom(w, &Geometry::Polygon(t.to_polygon()))?,
            Geometry::Line(l) => {
                write_geom(w, &Geometry::LineString(LineString::new(vec![l.start, l.end])))?
            }
        }
        Ok(())
    }

    fn read_order<R: Read>(r: &mut R) -> std::io::Result<()> {
        let order = r.read_u8()?;
        if order != LE {
            // We only emit LE; reject foreign byte order rather than mis-parse.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "only little-endian WKB supported",
            ));
        }
        Ok(())
    }

    fn read_ring<R: Read>(r: &mut R) -> std::io::Result<LineString<f64>> {
        let n = r.read_u32::<LittleEndian>()? as usize;
        let mut coords = std::vec::Vec::with_capacity(n);
        for _ in 0..n {
            let x = r.read_f64::<LittleEndian>()?;
            let y = r.read_f64::<LittleEndian>()?;
            coords.push(geo::Coord { x, y });
        }
        Ok(LineString::new(coords))
    }

    fn read_geom<R: Read>(r: &mut R) -> std::io::Result<Geometry<f64>> {
        read_order(r)?;
        let ty = r.read_u32::<LittleEndian>()?;
        Ok(match ty {
            1 => {
                let x = r.read_f64::<LittleEndian>()?;
                let y = r.read_f64::<LittleEndian>()?;
                Geometry::Point(Point::new(x, y))
            }
            2 => Geometry::LineString(read_ring(r)?),
            3 => {
                let nrings = r.read_u32::<LittleEndian>()? as usize;
                if nrings == 0 {
                    Geometry::Polygon(Polygon::new(LineString::new(vec![]), vec![]))
                } else {
                    let ext = read_ring(r)?;
                    let mut holes = std::vec::Vec::with_capacity(nrings - 1);
                    for _ in 1..nrings {
                        holes.push(read_ring(r)?);
                    }
                    Geometry::Polygon(Polygon::new(ext, holes))
                }
            }
            4 => {
                let n = r.read_u32::<LittleEndian>()? as usize;
                let mut pts = std::vec::Vec::with_capacity(n);
                for _ in 0..n {
                    if let Geometry::Point(p) = read_geom(r)? {
                        pts.push(p);
                    }
                }
                Geometry::MultiPoint(MultiPoint(pts))
            }
            5 => {
                let n = r.read_u32::<LittleEndian>()? as usize;
                let mut lss = std::vec::Vec::with_capacity(n);
                for _ in 0..n {
                    if let Geometry::LineString(ls) = read_geom(r)? {
                        lss.push(ls);
                    }
                }
                Geometry::MultiLineString(MultiLineString(lss))
            }
            6 => {
                let n = r.read_u32::<LittleEndian>()? as usize;
                let mut polys = std::vec::Vec::with_capacity(n);
                for _ in 0..n {
                    if let Geometry::Polygon(p) = read_geom(r)? {
                        polys.push(p);
                    }
                }
                Geometry::MultiPolygon(MultiPolygon(polys))
            }
            7 => {
                let n = r.read_u32::<LittleEndian>()? as usize;
                let mut subs = std::vec::Vec::with_capacity(n);
                for _ in 0..n {
                    subs.push(read_geom(r)?);
                }
                Geometry::GeometryCollection(GeometryCollection(subs))
            }
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    std::format!("unsupported WKB type {other}"),
                ))
            }
        })
    }
}

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
}
