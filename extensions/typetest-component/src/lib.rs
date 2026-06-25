//! Type de-risk fixture. Registers two zero-arg scalars exercising the NEW
//! logical types:
//!   tt_int32()     -> int32     (returns i32::MAX = 2147483647)
//!   tt_timestamp() -> timestamp (returns 1609459200000000 micros = 2021-01-01 00:00:00)
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

// 2021-01-01 00:00:00 UTC in microseconds since the epoch.
const FIXED_TS_MICROS: i64 = 1_609_459_200_000_000;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "typetest".into(),
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
        _args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            T::Int32 => types::Duckvalue::Int32(2147483647),
            T::Timestamp => types::Duckvalue::Timestamp(FIXED_TS_MICROS),
            T::Int8 => types::Duckvalue::Int8(127),
            T::Int16 => types::Duckvalue::Int16(32767),
            T::Uint8 => types::Duckvalue::Uint8(255),
            T::Uint16 => types::Duckvalue::Uint16(65535),
            T::Uint32 => types::Duckvalue::Uint32(4294967295),
            T::Float32 => types::Duckvalue::Float32(1.5),
            // 2021-01-01: 18628 days since 1970-01-01.
            T::Date => types::Duckvalue::Date(18628),
            // 12:34:56.789000 since midnight, in microseconds.
            T::Time => types::Duckvalue::Time(45_296_789_000),
            T::Timestamptz => types::Duckvalue::Timestamptz(FIXED_TS_MICROS),
            // DECIMAL(38,4) = 12345.6789 -> unscaled int128 123_456_789.
            T::Decimal => types::Duckvalue::Decimal(types::Decimalvalue {
                lower: 123_456_789,
                upper: 0,
                width: 38,
                scale: 4,
            }),
            // INTERVAL '1 month 2 days 3 seconds'.
            T::Interval => types::Duckvalue::Interval(types::Intervalvalue {
                months: 1,
                days: 2,
                micros: 3_000_000,
            }),
            // UUID 12345678-1234-5678-1234-567812345678.
            T::Uuid => types::Duckvalue::Uuid(types::Uuidvalue {
                hi: 0x1234_5678_1234_5678,
                lo: 0x1234_5678_1234_5678,
            }),
        })
    }
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("typetest: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("typetest: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("typetest: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("typetest: no casts".into()))
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

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::Int32);
    reg.register(
        "tt_int32",
        &[],
        types::Logicaltype::Int32,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("returns i32::MAX as INTEGER".into()),
            tags: vec!["typetest".into()],
            attributes: det,
        }),
    )?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::Timestamp);
    reg.register(
        "tt_timestamp",
        &[],
        types::Logicaltype::Timestamp,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("returns 2021-01-01 as TIMESTAMP".into()),
            tags: vec!["typetest".into()],
            attributes: det,
        }),
    )?;

    // Remaining fixed-width + temporal types. Each is a zero-arg scalar
    // returning a fixed value of the matching DuckDB type.
    let extras: &[(&str, T, types::Logicaltype, &str)] = &[
        ("tt_int8", T::Int8, types::Logicaltype::Int8, "returns 127 as TINYINT"),
        ("tt_int16", T::Int16, types::Logicaltype::Int16, "returns 32767 as SMALLINT"),
        ("tt_uint8", T::Uint8, types::Logicaltype::Uint8, "returns 255 as UTINYINT"),
        ("tt_uint16", T::Uint16, types::Logicaltype::Uint16, "returns 65535 as USMALLINT"),
        ("tt_uint32", T::Uint32, types::Logicaltype::Uint32, "returns 4294967295 as UINTEGER"),
        ("tt_float32", T::Float32, types::Logicaltype::Float32, "returns 1.5 as FLOAT"),
        ("tt_date", T::Date, types::Logicaltype::Date, "returns 2021-01-01 as DATE"),
        ("tt_time", T::Time, types::Logicaltype::Time, "returns 12:34:56.789 as TIME"),
        ("tt_timestamptz", T::Timestamptz, types::Logicaltype::Timestamptz, "returns 2021-01-01 as TIMESTAMP_TZ"),
        ("tt_decimal", T::Decimal, types::Logicaltype::Decimal, "returns 12345.6789 as DECIMAL"),
        ("tt_interval", T::Interval, types::Logicaltype::Interval, "returns 1 month 2 days 3s as INTERVAL"),
        ("tt_uuid", T::Uuid, types::Logicaltype::Uuid, "returns a fixed UUID"),
    ];
    for (name, tag, ty, desc) in extras.iter().copied() {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, tag);
        reg.register(
            name,
            &[],
            ty,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["typetest".into()],
                attributes: det,
            }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum T {
    Int32,
    Timestamp,
    Int8,
    Int16,
    Uint8,
    Uint16,
    Uint32,
    Float32,
    Date,
    Time,
    Timestamptz,
    Decimal,
    Interval,
    Uuid,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
