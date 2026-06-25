//! Read Apache Avro Object Container Files (OCF) as DuckDB table functions over
//! an in-memory BLOB.
//!
//! Reimplements the READ side of DuckDB's official `avro` extension as a
//! loadable ducklink component. The component table-function registry needs a
//! FIXED column list at registration time, but an Avro record's schema is
//! dynamic -- so `read_avro` uses the same MELTED shape as parquetfns /
//! sqlite_blob_scan: one (row_no, col, val) tuple per field.
//!
//!   avro_schema(data BLOB) -> table(
//!       field VARCHAR,   -- top-level field name (writer schema record fields)
//!       type  VARCHAR)   -- the field's Avro type rendered as text
//!
//!   read_avro(data BLOB) -> table(
//!       row_no BIGINT,   -- 0-indexed record ordinal
//!       col    VARCHAR,  -- field name
//!       val    VARCHAR)  -- the value rendered as text (JSON for nested; NULL stays NULL)
//!
//!   avro_record_count(data BLOB) -> BIGINT  -- number of decodable records (scalar)
//!
//! NOTE on names: these collide with the `avro` extension EMBEDDED in the
//! default DuckDB core, so this component only loads against a lean core that
//! has the embedded avro extension de-embedded.
//!
//! Every function accepts the OCF as a real BLOB or as a hex STRING (the wasm
//! core registers table-function params as VARCHAR, so the SQL entry point
//! passes hex which we decode). A malformed / empty / NULL blob yields ZERO rows
//! (or 0 for the count) -- never a panic and never an error.
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

use apache_avro::schema::Schema;
use apache_avro::types::Value;
use apache_avro::Reader;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_all()?;
        Ok(types::Loadresult {
            name: "avrofns".into(),
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
        rows: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        // The only scalar fn is avro_record_count(data) -> BIGINT.
        let mut out: std::vec::Vec<types::Duckvalue> = std::vec::Vec::with_capacity(rows.len());
        for args in rows {
            out.push(record_count_value(args.into_iter().next()));
        }
        Ok(out.into())
    }
    fn call_scalar(
        _h: u32,
        args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Ok(record_count_value(args.into_iter().next()))
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
        let bytes: std::vec::Vec<u8> = match decode_arg(args.into_iter().next()) {
            Some(b) => b,
            None => return Ok(Vec::new().into()),
        };

        let rows = match which {
            T::Schema => schema_rows(&bytes),
            T::Read => read_melted(&bytes),
        };
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("avrofns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("avrofns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("avrofns: no casts".into()))
    }
}

export!(Extension);

// ---------------------------------------------------------------------------
// Core readers (pure functions over `&[u8]`; unit-tested natively).
// ---------------------------------------------------------------------------

/// avro_schema: one (field, type) row per top-level field of the writer schema.
/// A non-record top-level schema (or malformed OCF) yields zero rows.
fn schema_rows(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let reader = match Reader::new(bytes) {
        Ok(r) => r,
        Err(_) => return std::vec::Vec::new(),
    };
    let schema = reader.writer_schema();
    let fields = match schema {
        Schema::Record(rec) => &rec.fields,
        _ => return std::vec::Vec::new(),
    };
    let mut out = std::vec::Vec::with_capacity(fields.len());
    for f in fields {
        out.push(vec![
            types::Duckvalue::Text(f.name.clone().into()),
            types::Duckvalue::Text(schema_type_name(&f.schema).into()),
        ]);
    }
    out
}

/// read_avro (MELTED): one (row_no, col, val) tuple per record field.
/// Iteration stops at the first undecodable record (prior rows are kept).
fn read_melted(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let reader = match Reader::new(bytes) {
        Ok(r) => r,
        Err(_) => return std::vec::Vec::new(),
    };

    let mut out = std::vec::Vec::new();
    let mut row_no: i64 = 0;
    for item in reader {
        let value = match item {
            Ok(v) => v,
            Err(_) => break, // keep rows decoded so far
        };
        match value {
            Value::Record(fields) => {
                for (name, v) in fields {
                    out.push(vec![
                        types::Duckvalue::Int64(row_no),
                        types::Duckvalue::Text(name.into()),
                        value_as_text(v),
                    ]);
                }
            }
            // Non-record top-level value: emit a single tuple under col "value".
            other => {
                out.push(vec![
                    types::Duckvalue::Int64(row_no),
                    types::Duckvalue::Text("value".into()),
                    value_as_text(other),
                ]);
            }
        }
        row_no += 1;
    }
    out
}

/// avro_record_count: count of decodable records. Malformed/empty/NULL -> 0.
fn record_count_value(arg: Option<types::Duckvalue>) -> types::Duckvalue {
    let bytes = match decode_arg(arg) {
        Some(b) => b,
        None => return types::Duckvalue::Int64(0),
    };
    let reader = match Reader::new(&bytes[..]) {
        Ok(r) => r,
        Err(_) => return types::Duckvalue::Int64(0),
    };
    let mut n: i64 = 0;
    for item in reader {
        match item {
            Ok(_) => n += 1,
            Err(_) => break, // count records decoded before the first failure
        }
    }
    types::Duckvalue::Int64(n)
}

/// Map an Avro `Schema` to a short text type label for `avro_schema`.
fn schema_type_name(s: &Schema) -> std::string::String {
    match s {
        Schema::Null => "null",
        Schema::Boolean => "boolean",
        Schema::Int => "int",
        Schema::Long => "long",
        Schema::Float => "float",
        Schema::Double => "double",
        Schema::Bytes => "bytes",
        Schema::String => "string",
        Schema::Array(_) => "array",
        Schema::Map(_) => "map",
        Schema::Union(_) => "union",
        Schema::Record(_) => "record",
        Schema::Enum(_) => "enum",
        Schema::Fixed(_) => "fixed",
        Schema::Decimal(_) => "decimal",
        Schema::BigDecimal => "big-decimal",
        Schema::Uuid => "uuid",
        Schema::Date => "date",
        Schema::TimeMillis => "time-millis",
        Schema::TimeMicros => "time-micros",
        Schema::TimestampMillis => "timestamp-millis",
        Schema::TimestampMicros => "timestamp-micros",
        Schema::TimestampNanos => "timestamp-nanos",
        Schema::LocalTimestampMillis => "local-timestamp-millis",
        Schema::LocalTimestampMicros => "local-timestamp-micros",
        Schema::LocalTimestampNanos => "local-timestamp-nanos",
        Schema::Duration => "duration",
        Schema::Ref { .. } => "ref",
    }
    .to_string()
}

/// Render an Avro `Value` as TEXT for the melted `val` slot.
///
/// Scalars are rendered plainly (no JSON quoting); nested values (Record /
/// Array / Map / Enum / etc.) are rendered as JSON. NULL maps to Duckvalue::Null.
/// Unions are unwrapped to their inner value. Bytes/Fixed are hex-encoded.
fn value_as_text(v: Value) -> types::Duckvalue {
    match v {
        Value::Null => types::Duckvalue::Null,
        Value::Boolean(b) => types::Duckvalue::Text(b.to_string().into()),
        Value::Int(i) => types::Duckvalue::Text(i.to_string().into()),
        Value::Long(l) => types::Duckvalue::Text(l.to_string().into()),
        Value::Float(f) => types::Duckvalue::Text(f.to_string().into()),
        Value::Double(d) => types::Duckvalue::Text(d.to_string().into()),
        Value::String(s) => types::Duckvalue::Text(s.into()),
        Value::Enum(_, s) => types::Duckvalue::Text(s.into()),
        Value::Bytes(b) | Value::Fixed(_, b) => types::Duckvalue::Text(hex_encode(&b).into()),
        Value::Date(d) => types::Duckvalue::Text(d.to_string().into()),
        Value::TimeMillis(t) => types::Duckvalue::Text(t.to_string().into()),
        Value::TimeMicros(t) => types::Duckvalue::Text(t.to_string().into()),
        Value::TimestampMillis(t)
        | Value::TimestampMicros(t)
        | Value::TimestampNanos(t)
        | Value::LocalTimestampMillis(t)
        | Value::LocalTimestampMicros(t)
        | Value::LocalTimestampNanos(t) => types::Duckvalue::Text(t.to_string().into()),
        Value::Uuid(u) => types::Duckvalue::Text(u.to_string().into()),
        // Unwrap the union to its inner branch, then render that.
        Value::Union(_, inner) => value_as_text(*inner),
        // Everything nested / structured -> JSON. NULL on any conversion failure.
        nested => match serde_json::Value::try_from(nested)
            .ok()
            .and_then(|j| serde_json::to_string(&j).ok())
        {
            Some(s) => types::Duckvalue::Text(s.into()),
            None => types::Duckvalue::Null,
        },
    }
}

/// Decode the single function argument into raw OCF bytes.
/// BLOB -> as-is; TEXT -> hex-decoded (the wasm SQL path passes hex). NULL /
/// absent / unparsable -> None (caller treats as zero rows / count 0).
fn decode_arg(arg: Option<types::Duckvalue>) -> Option<std::vec::Vec<u8>> {
    match arg {
        Some(types::Duckvalue::Blob(b)) => Some(b.into()),
        Some(types::Duckvalue::Text(s)) => hex_decode(&s),
        _ => None,
    }
}

/// Hex-encode bytes (lowercase) so binary cells stay printable.
fn hex_encode(b: &[u8]) -> std::string::String {
    let mut s = std::string::String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
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
    Read,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_all() -> Result<(), types::Duckerror> {
    // Table capability ----------------------------------------------------
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    // Every function takes the OCF as a single `data` argument. The wasm core
    // registers table-function params as VARCHAR, so callers pass a hex string;
    // we also accept a real BLOB for the native path.
    let data_arg = || {
        vec![runtime::Funcarg {
            name: Some("data".into()),
            logical: types::Logicaltype::Blob,
        }]
    };

    // avro_schema --------------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Schema);
        let columns = vec![
            types::Columndef { name: "field".into(), logical: types::Logicaltype::Text },
            types::Columndef { name: "type".into(), logical: types::Logicaltype::Text },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Avro writer-schema top-level fields: (field, type) per record field".into(),
            ),
            tags: vec!["avro".into(), "schema".into()],
        };
        reg.register(
            "avro_schema",
            &data_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // read_avro (MELTED) -------------------------------------------------
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
                "Read an Avro OCF BLOB, MELTING each record into (row_no, col, val) tuples \
                 (component table fns need fixed columns; avro schema is dynamic)"
                    .into(),
            ),
            tags: vec!["avro".into(), "read".into(), "melted".into()],
        };
        reg.register(
            "read_avro",
            &data_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // avro_record_count (scalar) -----------------------------------------
    // Optional: the scalar fn is a convenience and the table fns are what the
    // smoke exercises, so a missing capability / registration failure is fine.
    if let Some(runtime::Capability::Scalar(sreg)) =
        runtime::get_capability(types::Capabilitykind::Scalar)
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
        let _ = sreg.register(
            "avro_record_count",
            &data_arg(),
            &types::Logicaltype::Int64,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some("Count of decodable records in an Avro OCF BLOB".into()),
                tags: vec!["avro".into(), "count".into()],
                attributes: det,
            }),
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native tests: build a tiny Avro OCF in-memory, then drive the readers.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use apache_avro::{Schema as AvroSchema, Writer};

    /// A {a: long, b: string} record schema with 2 records:
    ///   a=[1, 2]  b=["x", "y"]
    fn make_avro() -> std::vec::Vec<u8> {
        let raw = r#"
            {
              "type": "record",
              "name": "t",
              "fields": [
                {"name": "a", "type": "long"},
                {"name": "b", "type": "string"}
              ]
            }
        "#;
        let schema = AvroSchema::parse_str(raw).unwrap();
        let mut writer = Writer::new(&schema, std::vec::Vec::new());

        let mut r1 = apache_avro::types::Record::new(writer.schema()).unwrap();
        r1.put("a", 1i64);
        r1.put("b", "x");
        writer.append(r1).unwrap();

        let mut r2 = apache_avro::types::Record::new(writer.schema()).unwrap();
        r2.put("a", 2i64);
        r2.put("b", "y");
        writer.append(r2).unwrap();

        writer.into_inner().unwrap()
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
    fn schema_lists_fields_and_types() {
        let bytes = make_avro();
        let rows = schema_rows(&bytes);
        let got: std::vec::Vec<(std::string::String, std::string::String)> = rows
            .iter()
            .map(|r| (as_text(&r[0]), as_text(&r[1])))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "long".to_string()),
                ("b".to_string(), "string".to_string()),
            ]
        );
    }

    #[test]
    fn melted_read_emits_one_tuple_per_field() {
        let bytes = make_avro();
        let rows = read_melted(&bytes);
        // 2 records * 2 fields = 4 melted tuples.
        assert_eq!(rows.len(), 4);

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
        assert_eq!(triples[1], (0, "b".to_string(), "x".to_string()));
        assert_eq!(triples[2], (1, "a".to_string(), "2".to_string()));
        assert_eq!(triples[3], (1, "b".to_string(), "y".to_string()));
    }

    #[test]
    fn record_count_counts_records() {
        let bytes = make_avro();
        let c = record_count_value(Some(types::Duckvalue::Blob(bytes.into())));
        assert_eq!(as_text(&c), "2");
    }

    #[test]
    fn malformed_blob_is_empty_never_panics() {
        assert!(schema_rows(b"not an avro file").is_empty());
        assert!(read_melted(b"not an avro file").is_empty());
        assert!(schema_rows(b"").is_empty());
        assert!(read_melted(b"").is_empty());
        assert_eq!(
            as_text(&record_count_value(Some(types::Duckvalue::Blob(
                b"nope".to_vec().into()
            )))),
            "0"
        );
        assert_eq!(as_text(&record_count_value(None)), "0");
        assert_eq!(
            as_text(&record_count_value(Some(types::Duckvalue::Null))),
            "0"
        );
    }

    #[test]
    fn hex_roundtrips() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(hex_decode("0").is_none()); // odd length
        assert!(hex_decode("zz").is_none()); // bad char
        assert_eq!(hex_encode(&[0x00, 0xff, 0x10]), "00ff10");
    }

    /// hex path: decode_arg(TEXT hex of the OCF) drives the same readers.
    #[test]
    fn hex_text_arg_decodes_to_bytes() {
        let bytes = make_avro();
        let hexed = hex_encode(&bytes);
        let decoded = decode_arg(Some(types::Duckvalue::Text(hexed.into()))).unwrap();
        assert_eq!(decoded, bytes);
        assert_eq!(read_melted(&decoded).len(), 4);
    }

    /// Helper to regenerate the hex blob embedded in smoke.sql. Ignored by
    /// default; run with:
    ///   cargo test --release dump_fixture_hex -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_fixture_hex() {
        let bytes = make_avro();
        println!("AVRO_HEX={}", hex_encode(&bytes));
    }
}
