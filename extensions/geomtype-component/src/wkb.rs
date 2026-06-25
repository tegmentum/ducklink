//! Self-contained WKB codec (little-endian, ISO/OGC standard WKB).
//!   byte order: 1 (LE)
//!   type code:  1=Point 2=LineString 3=Polygon 4=MultiPoint
//!               5=MultiLineString 6=MultiPolygon 7=GeometryCollection
//! 2D doubles only (no Z/M, no SRID) — sufficient for the geometry proof.
//!
//! This module is intentionally WIT-FREE (depends only on `geo`, `byteorder`
//! and `std`) so it can be `#[path]`-included by both the wasm component
//! (`mod wkb;`) and the native fuzz target. `decode` NEVER panics on adversarial
//! bytes: out-of-range counts are capacity-capped, foreign byte order and
//! unknown type codes return `Err`, and deep nesting is depth-limited.
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use geo::geometry::Geometry;
use geo::geometry::{
    GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use std::io::{Cursor, Read, Write};

const LE: u8 = 1;

/// Cap on the up-front capacity we reserve from an untrusted WKB count field
/// (a u32, so up to ~4 billion). `Vec::with_capacity` on such a value would
/// abort the process with a capacity-overflow / OOM before a single
/// coordinate is read. We still read every element via `push`, so a genuinely
/// large (but well-formed) geometry is decoded correctly; a truncated one
/// simply errors out partway. This only bounds the *initial* reservation.
const MAX_PREALLOC: usize = 4096;

pub fn encode(g: &Geometry<f64>) -> Option<std::vec::Vec<u8>> {
    let mut buf: std::vec::Vec<u8> = std::vec::Vec::new();
    write_geom(&mut buf, g).ok()?;
    Some(buf)
}

/// Max nesting depth for (Multi*/GeometryCollection) recursion. A maliciously
/// nested WKB (a GeometryCollection of a GeometryCollection of ...) would
/// otherwise recurse until the native stack overflows (an uncatchable abort).
const MAX_DEPTH: u32 = 64;

pub fn decode(bytes: &[u8]) -> Option<Geometry<f64>> {
    let mut c = Cursor::new(bytes);
    read_geom(&mut c, 0).ok()
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
    let mut coords = std::vec::Vec::with_capacity(n.min(MAX_PREALLOC));
    for _ in 0..n {
        let x = r.read_f64::<LittleEndian>()?;
        let y = r.read_f64::<LittleEndian>()?;
        coords.push(geo::Coord { x, y });
    }
    Ok(LineString::new(coords))
}

fn read_geom<R: Read>(r: &mut R, depth: u32) -> std::io::Result<Geometry<f64>> {
    if depth > MAX_DEPTH {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "WKB nesting too deep",
        ));
    }
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
                let mut holes = std::vec::Vec::with_capacity((nrings - 1).min(MAX_PREALLOC));
                for _ in 1..nrings {
                    holes.push(read_ring(r)?);
                }
                Geometry::Polygon(Polygon::new(ext, holes))
            }
        }
        4 => {
            let n = r.read_u32::<LittleEndian>()? as usize;
            let mut pts = std::vec::Vec::with_capacity(n.min(MAX_PREALLOC));
            for _ in 0..n {
                if let Geometry::Point(p) = read_geom(r, depth + 1)? {
                    pts.push(p);
                }
            }
            Geometry::MultiPoint(MultiPoint(pts))
        }
        5 => {
            let n = r.read_u32::<LittleEndian>()? as usize;
            let mut lss = std::vec::Vec::with_capacity(n.min(MAX_PREALLOC));
            for _ in 0..n {
                if let Geometry::LineString(ls) = read_geom(r, depth + 1)? {
                    lss.push(ls);
                }
            }
            Geometry::MultiLineString(MultiLineString(lss))
        }
        6 => {
            let n = r.read_u32::<LittleEndian>()? as usize;
            let mut polys = std::vec::Vec::with_capacity(n.min(MAX_PREALLOC));
            for _ in 0..n {
                if let Geometry::Polygon(p) = read_geom(r, depth + 1)? {
                    polys.push(p);
                }
            }
            Geometry::MultiPolygon(MultiPolygon(polys))
        }
        7 => {
            let n = r.read_u32::<LittleEndian>()? as usize;
            let mut subs = std::vec::Vec::with_capacity(n.min(MAX_PREALLOC));
            for _ in 0..n {
                subs.push(read_geom(r, depth + 1)?);
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
