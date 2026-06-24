//! Uber H3 hexagonal geospatial indexing as DuckDB scalars (via `h3o`):
//!   h3_latlng_to_cell(lat, lng, res) -> BIGINT,
//!   h3_cell_to_lat(cell) -> DOUBLE, h3_cell_to_lng(cell) -> DOUBLE,
//!   h3_cell_to_parent(cell, res) -> BIGINT,
//!   h3_grid_distance(a, b) -> BIGINT,
//!   h3_is_valid_cell(cell) -> BOOLEAN.
//! NULL / invalid input -> NULL (never panics).
use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use h3o::{CellIndex, LatLng, Resolution};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "h3".into(),
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

fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        _ => None,
    }
}

fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        _ => None,
    }
}

/// Parse a BIGINT argument into a valid H3 cell index.
/// The i64 is bit-reinterpreted as u64 (valid H3 indices have bit 63 clear,
/// so they are always non-negative i64 values).
fn cell_arg(args: &[types::Duckvalue], i: usize) -> Option<CellIndex> {
    let raw = i64_arg(args, i)?;
    CellIndex::try_from(raw as u64).ok()
}

fn res_arg(args: &[types::Duckvalue], i: usize) -> Option<Resolution> {
    let raw = i64_arg(args, i)?;
    let r = u8::try_from(raw).ok()?;
    Resolution::try_from(r).ok()
}

fn cell_to_i64(cell: CellIndex) -> i64 {
    u64::from(cell) as i64
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
        Ok(match which {
            H::LatLngToCell => {
                match (f64_arg(&args, 0), f64_arg(&args, 1), res_arg(&args, 2)) {
                    (Some(lat), Some(lng), Some(res)) => match LatLng::new(lat, lng) {
                        Ok(ll) => types::Duckvalue::Int64(cell_to_i64(ll.to_cell(res))),
                        Err(_) => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            H::CellToLat | H::CellToLng => match cell_arg(&args, 0) {
                Some(cell) => {
                    let ll: LatLng = cell.into();
                    types::Duckvalue::Float64(if which == H::CellToLat {
                        ll.lat()
                    } else {
                        ll.lng()
                    })
                }
                None => types::Duckvalue::Null,
            },
            H::CellToParent => match (cell_arg(&args, 0), res_arg(&args, 1)) {
                (Some(cell), Some(res)) => match cell.parent(res) {
                    Some(p) => types::Duckvalue::Int64(cell_to_i64(p)),
                    None => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
            H::GridDistance => match (cell_arg(&args, 0), cell_arg(&args, 1)) {
                (Some(a), Some(b)) => match a.grid_distance(b) {
                    Ok(d) => types::Duckvalue::Int64(d.into()),
                    Err(_) => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
            H::IsValidCell => {
                let valid = match i64_arg(&args, 0) {
                    Some(raw) => CellIndex::try_from(raw as u64).is_ok(),
                    None => false,
                };
                types::Duckvalue::Boolean(valid)
            }
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("h3: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("h3: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("h3: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("h3: no casts".into()))
    }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    fn arg(name: &str, logical: types::Logicaltype) -> runtime::Funcarg {
        runtime::Funcarg {
            name: Some(name.into()),
            logical,
        }
    }
    fn opts(desc: &str, det: types::Funcflags) -> runtime::Funcopts {
        runtime::Funcopts {
            description: Some(desc.into()),
            tags: vec!["geo".into(), "h3".into()],
            attributes: det,
        }
    }

    // h3_latlng_to_cell(lat DOUBLE, lng DOUBLE, res INTEGER) -> BIGINT
    let h = next(H::LatLngToCell);
    reg.register(
        "h3_latlng_to_cell",
        &[
            arg("lat", types::Logicaltype::Float64),
            arg("lng", types::Logicaltype::Float64),
            arg("res", types::Logicaltype::Int64),
        ],
        types::Logicaltype::Int64,
        runtime::ScalarCallback::new(h),
        Some(&opts("lat/lng/res -> H3 cell index", det)),
    )?;

    // h3_cell_to_lat(cell BIGINT) -> DOUBLE / h3_cell_to_lng(cell BIGINT) -> DOUBLE
    for (name, g, desc) in [
        ("h3_cell_to_lat", H::CellToLat, "H3 cell -> center latitude"),
        ("h3_cell_to_lng", H::CellToLng, "H3 cell -> center longitude"),
    ] {
        let h = next(g);
        reg.register(
            name,
            &[arg("cell", types::Logicaltype::Int64)],
            types::Logicaltype::Float64,
            runtime::ScalarCallback::new(h),
            Some(&opts(desc, det)),
        )?;
    }

    // h3_cell_to_parent(cell BIGINT, res INTEGER) -> BIGINT
    let h = next(H::CellToParent);
    reg.register(
        "h3_cell_to_parent",
        &[
            arg("cell", types::Logicaltype::Int64),
            arg("res", types::Logicaltype::Int64),
        ],
        types::Logicaltype::Int64,
        runtime::ScalarCallback::new(h),
        Some(&opts("H3 cell -> parent at resolution", det)),
    )?;

    // h3_grid_distance(a BIGINT, b BIGINT) -> BIGINT
    let h = next(H::GridDistance);
    reg.register(
        "h3_grid_distance",
        &[
            arg("a", types::Logicaltype::Int64),
            arg("b", types::Logicaltype::Int64),
        ],
        types::Logicaltype::Int64,
        runtime::ScalarCallback::new(h),
        Some(&opts("grid distance between two H3 cells", det)),
    )?;

    // h3_is_valid_cell(cell BIGINT) -> BOOLEAN
    let h = next(H::IsValidCell);
    reg.register(
        "h3_is_valid_cell",
        &[arg("cell", types::Logicaltype::Int64)],
        types::Logicaltype::Boolean,
        runtime::ScalarCallback::new(h),
        Some(&opts("is the BIGINT a valid H3 cell index", det)),
    )?;

    Ok(())
}

fn next(g: H) -> u32 {
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, g);
    h
}

#[derive(Clone, Copy, PartialEq)]
enum H {
    LatLngToCell,
    CellToLat,
    CellToLng,
    CellToParent,
    GridDistance,
    IsValidCell,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
