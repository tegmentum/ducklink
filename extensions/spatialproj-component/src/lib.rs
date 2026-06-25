//! ST_Transform(geom_wkt VARCHAR, from_srid INTEGER, to_srid INTEGER) -> VARCHAR
//!
//! Reprojects a WKT geometry from one EPSG CRS to another by COMPOSING the
//! prebuilt GDAL component: this duckdb:extension imports `gdal:core/srs` and
//! calls spatial-ref::from-epsg + the coordinate-transform resource. EPSG codes
//! are resolved by PROJ's proj.db, which is embedded inside the gdal component
//! (no host filesystem needed). NULL on any error.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "spatialproj", generate_all });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
// The composed GDAL dependency.
use gdal::core::srs;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "spatialproj".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn i32_arg(args: &[types::Duckvalue], i: usize) -> Option<i32> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v as i32),
        Some(types::Duckvalue::Int32(v)) => Some(*v),
        _ => None,
    }
}

/// Traditional GIS axis order (lon, lat) so WKT x=lon / y=lat flows straight
/// through PROJ. (GDAL's OAMS_TRADITIONAL_GIS_ORDER == 0.)
const TRADITIONAL_GIS_ORDER: u32 = 0;

/// Reproject one WKT geometry string. Returns None on any failure (-> SQL NULL).
fn transform_wkt(wkt_in: &str, from_srid: i32, to_srid: i32) -> Option<std::string::String> {
    use std::str::FromStr;
    if from_srid <= 0 || to_srid <= 0 {
        return None;
    }

    // Build source + target spatial references from EPSG codes (embedded proj.db).
    let src = srs::SpatialRef::from_epsg(from_srid as u32).ok()?;
    src.set_axis_mapping_strategy(TRADITIONAL_GIS_ORDER);
    let dst = srs::SpatialRef::from_epsg(to_srid as u32).ok()?;
    dst.set_axis_mapping_strategy(TRADITIONAL_GIS_ORDER);

    // Coordinate transformation between the two CRS.
    let xform = srs::Transform::new(&src, &dst);

    // Parse WKT -> geo-types geometry, walk every coordinate, reproject in place.
    let geom = wkt::Wkt::<f64>::from_str(wkt_in).ok()?;
    let mut g: geo_types::Geometry<f64> = geom.try_into().ok()?;
    reproject_geometry(&mut g, &xform)?;

    // Re-emit as WKT.
    use wkt::ToWkt;
    Some(g.wkt_string())
}

/// Reproject every coordinate of a geo-types geometry through the transform.
fn reproject_geometry(g: &mut geo_types::Geometry<f64>, xform: &srs::Transform) -> Option<()> {
    use geo_types::Geometry::*;
    match g {
        Point(p) => reproject_coord(&mut p.0, xform),
        Line(l) => { reproject_coord(&mut l.start, xform)?; reproject_coord(&mut l.end, xform) }
        LineString(ls) => reproject_coords(ls.0.iter_mut(), xform),
        Polygon(poly) => reproject_polygon(poly, xform),
        MultiPoint(mp) => {
            for p in mp.0.iter_mut() { reproject_coord(&mut p.0, xform)?; }
            Some(())
        }
        MultiLineString(mls) => {
            for ls in mls.0.iter_mut() { reproject_coords(ls.0.iter_mut(), xform)?; }
            Some(())
        }
        MultiPolygon(mpoly) => {
            for poly in mpoly.0.iter_mut() { reproject_polygon(poly, xform)?; }
            Some(())
        }
        GeometryCollection(gc) => {
            for inner in gc.0.iter_mut() { reproject_geometry(inner, xform)?; }
            Some(())
        }
        Rect(_) | Triangle(_) => None,
    }
}

fn reproject_polygon(poly: &mut geo_types::Polygon<f64>, xform: &srs::Transform) -> Option<()> {
    // geo-types Polygon fields are private; rebuild from reprojected rings.
    let mut ext: geo_types::LineString<f64> = poly.exterior().clone();
    reproject_coords(ext.0.iter_mut(), xform)?;
    let mut ints: std::vec::Vec<geo_types::LineString<f64>> = poly.interiors().to_vec();
    for ring in ints.iter_mut() {
        reproject_coords(ring.0.iter_mut(), xform)?;
    }
    *poly = geo_types::Polygon::new(ext, ints);
    Some(())
}

fn reproject_coords<'a, I>(coords: I, xform: &srs::Transform) -> Option<()>
where
    I: Iterator<Item = &'a mut geo_types::Coord<f64>>,
{
    for c in coords {
        reproject_coord(c, xform)?;
    }
    Some(())
}

fn reproject_coord(c: &mut geo_types::Coord<f64>, xform: &srs::Transform) -> Option<()> {
    let (x, y, _z) = xform.transform_point(c.x, c.y, 0.0).ok()?;
    c.x = x;
    c.y = y;
    Some(())
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let _which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let wkt_in = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let from = match i32_arg(&args, 1) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        let to = match i32_arg(&args, 2) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        Ok(match transform_wkt(&wkt_in, from, to) {
            Some(s) => types::Duckvalue::Text(s.into()),
            None => types::Duckvalue::Null,
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("spatialproj: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("spatialproj: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("spatialproj: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("spatialproj: no casts".into())) }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, S::Transform);
    reg.register("ST_Transform", &[
        runtime::Funcarg { name: Some("geom_wkt".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("from_srid".into()), logical: types::Logicaltype::Int32 },
        runtime::Funcarg { name: Some("to_srid".into()), logical: types::Logicaltype::Int32 }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("Reproject a WKT geometry between EPSG CRS via composed GDAL/PROJ".into()),
            tags: vec!["geo".into(), "gdal".into()],
            attributes: det,
        }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum S { Transform }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, S>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, S>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
