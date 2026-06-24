//! Read Apache Parquet files as DuckDB table functions over an in-memory BLOB.
//!
//! Reimplements the feasible parts of DuckDB's official `parquet` extension as
//! a loadable ducklink component. The component table-function registry needs a
//! FIXED column list at registration time, but a parquet file's schema is
//! dynamic -- so `read_parquet` uses the same MELTED shape as
//! `sqlite_blob_scan`: one (row_no, col, val) tuple per cell.
//!
//!   parquet_schema(data BLOB) -> table(
//!       column_name VARCHAR,   -- leaf column name
//!       column_type VARCHAR)   -- physical/logical type rendered as text
//!
//!   parquet_metadata(data BLOB) -> table(
//!       num_rows       BIGINT,
//!       num_columns    BIGINT,
//!       num_row_groups BIGINT,
//!       created_by     VARCHAR,
//!       version        BIGINT)
//!
//!   read_parquet(data BLOB) -> table(
//!       row_no BIGINT,         -- 0-indexed row ordinal
//!       col    VARCHAR,        -- leaf column name
//!       val    VARCHAR)        -- the cell value rendered as text (NULL stays NULL)
//!
//! NOTE on names: these collide with the parquet functions EMBEDDED in the
//! current DuckDB core, so this component only loads against a lean core that
//! has the embedded parquet extension removed.
//!
//! All three functions accept the parquet file as a real BLOB or as a hex
//! STRING (the wasm core registers table-function params as VARCHAR, so the SQL
//! entry point passes hex which we decode). A malformed / empty / NULL blob
//! yields ZERO rows -- never a panic and never an error.
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

use parquet::basic::Type as PhysicalType;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_all()?;
        Ok(types::Loadresult {
            name: "parquetfns".into(),
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
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("parquetfns: no scalar fns".into()))
    }
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("parquetfns: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        // Decode the single argument into bytes. A NULL / absent / non-BLOB,
        // non-hex argument yields zero rows (never an error) so malformed input
        // is tolerated end-to-end.
        let bytes = match args.into_iter().next() {
            Some(types::Duckvalue::Blob(b)) => Some(b.into()),
            Some(types::Duckvalue::Text(s)) => hex_decode(&s),
            _ => None,
        };
        let bytes: std::vec::Vec<u8> = match bytes {
            Some(b) => b,
            None => return Ok(Vec::new().into()),
        };

        let rows = match which {
            T::Schema => schema_rows(&bytes),
            T::Metadata => metadata_rows(&bytes),
            T::Read => read_melted(&bytes),
        };
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("parquetfns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("parquetfns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("parquetfns: no casts".into()))
    }
}

export!(Extension);

// ---------------------------------------------------------------------------
// Core readers (pure functions over `&[u8]`; unit-tested natively).
// ---------------------------------------------------------------------------

/// Open a parquet blob. Returns None on any malformed input (never panics).
fn open(bytes: &[u8]) -> Option<SerializedFileReader<bytes::Bytes>> {
    let b = bytes::Bytes::copy_from_slice(bytes);
    SerializedFileReader::new(b).ok()
}

/// parquet_schema: one (column_name, column_type) row per leaf column.
fn schema_rows(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let reader = match open(bytes) {
        Some(r) => r,
        None => return std::vec::Vec::new(),
    };
    let schema = reader.metadata().file_metadata().schema_descr();
    let mut out = std::vec::Vec::with_capacity(schema.num_columns());
    for i in 0..schema.num_columns() {
        let col = schema.column(i);
        out.push(vec![
            types::Duckvalue::Text(col.name().to_string().into()),
            types::Duckvalue::Text(physical_type_name(col.physical_type()).into()),
        ]);
    }
    out
}

/// parquet_metadata: a single fixed-schema summary row.
fn metadata_rows(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let reader = match open(bytes) {
        Some(r) => r,
        None => return std::vec::Vec::new(),
    };
    let md = reader.metadata();
    let fmd = md.file_metadata();
    let created_by = match fmd.created_by() {
        Some(s) => types::Duckvalue::Text(s.to_string().into()),
        None => types::Duckvalue::Null,
    };
    vec![vec![
        types::Duckvalue::Int64(fmd.num_rows()),
        types::Duckvalue::Int64(fmd.schema_descr().num_columns() as i64),
        types::Duckvalue::Int64(md.num_row_groups() as i64),
        created_by,
        types::Duckvalue::Int64(fmd.version() as i64),
    ]]
}

/// read_parquet (MELTED): one (row_no, col, val) tuple per cell.
fn read_melted(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let reader = match open(bytes) {
        Some(r) => r,
        None => return std::vec::Vec::new(),
    };
    let iter = match reader.get_row_iter(None) {
        Ok(it) => it,
        Err(_) => return std::vec::Vec::new(),
    };

    let mut out = std::vec::Vec::new();
    let mut row_no: i64 = 0;
    for row in iter {
        let row = match row {
            Ok(r) => r,
            Err(_) => break, // keep rows read so far
        };
        for (name, field) in row.get_column_iter() {
            out.push(vec![
                types::Duckvalue::Int64(row_no),
                types::Duckvalue::Text(name.clone().into()),
                field_as_text(field),
            ]);
        }
        row_no += 1;
    }
    out
}

/// Render a physical parquet type as a short text label.
fn physical_type_name(t: PhysicalType) -> std::string::String {
    match t {
        PhysicalType::BOOLEAN => "BOOLEAN",
        PhysicalType::INT32 => "INT32",
        PhysicalType::INT64 => "INT64",
        PhysicalType::INT96 => "INT96",
        PhysicalType::FLOAT => "FLOAT",
        PhysicalType::DOUBLE => "DOUBLE",
        PhysicalType::BYTE_ARRAY => "BYTE_ARRAY",
        PhysicalType::FIXED_LEN_BYTE_ARRAY => "FIXED_LEN_BYTE_ARRAY",
    }
    .to_string()
}

/// Render any parquet record `Field` as TEXT for the melted `val` slot.
/// NULL fields map to Duckvalue::Null; bytes are hex-encoded so the slot stays
/// printable; everything else uses the crate's Display.
fn field_as_text(f: &Field) -> types::Duckvalue {
    match f {
        Field::Null => types::Duckvalue::Null,
        Field::Bytes(b) => {
            let mut s = std::string::String::with_capacity(b.data().len() * 2);
            for byte in b.data() {
                s.push_str(&format!("{byte:02x}"));
            }
            types::Duckvalue::Text(s.into())
        }
        Field::Str(s) => types::Duckvalue::Text(s.clone().into()),
        other => types::Duckvalue::Text(other.to_string().into()),
    }
}

/// Decode an ASCII hex string into bytes; None on any invalid char / odd length.
fn hex_decode(s: &str) -> Option<std::vec::Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut out = std::vec::Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        out.push((nib(b[i])? << 4) | nib(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Registration + handle dispatch.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum T {
    Schema,
    Metadata,
    Read,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_all() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    // Every function takes the parquet file as a single `data` argument. The
    // wasm core registers table-function params as VARCHAR, so callers pass a
    // hex string; we also accept a real BLOB for the native path.
    let data_arg = || {
        vec![runtime::Funcarg {
            name: Some("data".into()),
            logical: types::Logicaltype::Blob,
        }]
    };

    // parquet_schema -----------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Schema);
        let columns = vec![
            types::Columndef { name: "column_name".into(), logical: types::Logicaltype::Text },
            types::Columndef { name: "column_type".into(), logical: types::Logicaltype::Text },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Parquet leaf-column schema: (column_name, column_type) per column".into(),
            ),
            tags: vec!["parquet".into(), "schema".into()],
        };
        reg.register(
            "parquet_schema",
            &data_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // parquet_metadata ---------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Metadata);
        let columns = vec![
            types::Columndef { name: "num_rows".into(), logical: types::Logicaltype::Int64 },
            types::Columndef { name: "num_columns".into(), logical: types::Logicaltype::Int64 },
            types::Columndef { name: "num_row_groups".into(), logical: types::Logicaltype::Int64 },
            types::Columndef { name: "created_by".into(), logical: types::Logicaltype::Text },
            types::Columndef { name: "version".into(), logical: types::Logicaltype::Int64 },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Parquet file metadata: num_rows, num_columns, num_row_groups, created_by, version"
                    .into(),
            ),
            tags: vec!["parquet".into(), "metadata".into()],
        };
        reg.register(
            "parquet_metadata",
            &data_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // read_parquet (MELTED) ---------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Read);
        let columns = vec![
            types::Columndef { name: "row_no".into(), logical: types::Logicaltype::Int64 },
            types::Columndef { name: "col".into(), logical: types::Logicaltype::Text },
            types::Columndef { name: "val".into(), logical: types::Logicaltype::Text },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Read a Parquet BLOB, MELTING each row into (row_no, col, val) tuples \
                 (component table fns need fixed columns; parquet schema is dynamic)"
                    .into(),
            ),
            tags: vec!["parquet".into(), "read".into(), "melted".into()],
        };
        reg.register(
            "read_parquet",
            &data_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native tests: build a tiny parquet file in-memory, then drive the readers.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use parquet::data_type::{ByteArray, ByteArrayType, Int32Type};
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    /// A table with 2 int32 columns (a, b) + 1 string column (s), 2 rows:
    ///   a=[1,2]  b=[10,20]  s=["x","y"]
    fn make_parquet() -> std::vec::Vec<u8> {
        let message_type = "
            message schema {
                REQUIRED INT32 a;
                REQUIRED INT32 b;
                REQUIRED BYTE_ARRAY s (UTF8);
            }
        ";
        let schema = Arc::new(parse_message_type(message_type).unwrap());
        let props = Arc::new(WriterProperties::builder().build());
        let mut buf: std::vec::Vec<u8> = std::vec::Vec::new();
        {
            let mut writer = SerializedFileWriter::new(&mut buf, schema, props).unwrap();
            let mut rg = writer.next_row_group().unwrap();

            // column a
            let mut c = rg.next_column().unwrap().unwrap();
            c.typed::<Int32Type>().write_batch(&[1, 2], None, None).unwrap();
            c.close().unwrap();
            // column b
            let mut c = rg.next_column().unwrap().unwrap();
            c.typed::<Int32Type>().write_batch(&[10, 20], None, None).unwrap();
            c.close().unwrap();
            // column s
            let mut c = rg.next_column().unwrap().unwrap();
            let vals = [ByteArray::from("x"), ByteArray::from("y")];
            c.typed::<ByteArrayType>().write_batch(&vals, None, None).unwrap();
            c.close().unwrap();

            rg.close().unwrap();
            writer.close().unwrap();
        }
        buf
    }

    fn as_text(v: &types::Duckvalue) -> std::string::String {
        match v {
            types::Duckvalue::Text(s) => s.to_string(),
            types::Duckvalue::Int64(i) => i.to_string(),
            types::Duckvalue::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn schema_lists_columns_and_types() {
        let bytes = make_parquet();
        let rows = schema_rows(&bytes);
        assert_eq!(rows.len(), 3, "expected 3 leaf columns");
        let got: std::vec::Vec<(std::string::String, std::string::String)> = rows
            .iter()
            .map(|r| (as_text(&r[0]), as_text(&r[1])))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "INT32".to_string()),
                ("b".to_string(), "INT32".to_string()),
                ("s".to_string(), "BYTE_ARRAY".to_string()),
            ]
        );
    }

    #[test]
    fn metadata_summary_row() {
        let bytes = make_parquet();
        let rows = metadata_rows(&bytes);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(as_text(&r[0]), "2"); // num_rows
        assert_eq!(as_text(&r[1]), "3"); // num_columns
        assert_eq!(as_text(&r[2]), "1"); // num_row_groups
    }

    #[test]
    fn melted_read_emits_one_tuple_per_cell() {
        let bytes = make_parquet();
        let rows = read_melted(&bytes);
        // 2 rows * 3 cols = 6 melted tuples.
        assert_eq!(rows.len(), 6);

        let triples: std::vec::Vec<(i64, std::string::String, std::string::String)> = rows
            .iter()
            .map(|r| {
                let rn = match &r[0] {
                    types::Duckvalue::Int64(i) => *i,
                    _ => panic!("row_no not int"),
                };
                (rn, as_text(&r[1]), as_text(&r[2]))
            })
            .collect();

        assert_eq!(triples[0], (0, "a".to_string(), "1".to_string()));
        assert_eq!(triples[1], (0, "b".to_string(), "10".to_string()));
        assert_eq!(triples[2], (0, "s".to_string(), "x".to_string()));
        assert_eq!(triples[3], (1, "a".to_string(), "2".to_string()));
        assert_eq!(triples[4], (1, "b".to_string(), "20".to_string()));
        assert_eq!(triples[5], (1, "s".to_string(), "y".to_string()));
    }

    #[test]
    fn malformed_blob_is_empty_never_panics() {
        assert!(schema_rows(b"not a parquet file").is_empty());
        assert!(metadata_rows(b"not a parquet file").is_empty());
        assert!(read_melted(b"not a parquet file").is_empty());
        assert!(schema_rows(b"").is_empty());
        assert!(read_melted(b"").is_empty());
    }

    #[test]
    fn hex_decode_roundtrips() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(hex_decode("0").is_none()); // odd length
        assert!(hex_decode("zz").is_none()); // bad char
    }

    /// Helper to regenerate the hex blob embedded in smoke.sql. Ignored by
    /// default; run with:
    ///   cargo test --release dump_fixture_hex -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_fixture_hex() {
        let bytes = make_parquet();
        let mut s = std::string::String::with_capacity(bytes.len() * 2);
        for b in &bytes {
            s.push_str(&format!("{b:02x}"));
        }
        println!("PARQUET_HEX={}", s);
    }
}
