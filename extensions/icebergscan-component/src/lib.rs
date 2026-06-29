//! Inspect Apache Iceberg table metadata as DuckDB table functions.
//!
//! An Iceberg table on disk is multi-file:
//!   metadata/vNNN.metadata.json   -- the table metadata (schema, snapshots, ...)
//!     -> snapshots[].manifest-list -> a manifest-list (Avro)
//!         -> manifests (Avro)       -> data-file list
//!             -> Parquet data files
//! This component reads ONLY the top metadata.json (pure JSON) -- a metadata
//! inspector. Following the manifest-list / manifests / Parquet needs host file
//! access and is out of scope (see the composition note at the bottom of the
//! crate description / report).
//!
//!   iceberg_metadata(metadata_json VARCHAR) -> table(
//!       key   VARCHAR,   -- format-version, table-uuid, current-snapshot-id, ...
//!       value VARCHAR)
//!   iceberg_schema(metadata_json VARCHAR) -> table(
//!       field_id BIGINT,
//!       name     VARCHAR,
//!       type     VARCHAR,
//!       required BOOLEAN)   -- fields of the current schema (current-schema-id)
//!   iceberg_snapshots(metadata_json VARCHAR) -> table(
//!       snapshot_id     BIGINT,
//!       sequence_number BIGINT,
//!       timestamp_ms    BIGINT,
//!       manifest_list   VARCHAR)
//!
//! NULL / non-text / unparsable JSON yields zero rows -- never a panic.
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

use serde_json::Value;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_all()?;
        Ok(types::Loadresult {
            name: "icebergscan".into(),
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
    // major-4 columnar dispatch: icebergscan is a table-only component, so the
    // three columnar hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "icebergscan: no scalar fns".into(),
        ))
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

        // The single argument is the metadata.json text. A NULL / absent / non-text
        // argument yields zero rows (never an error) so bad input is tolerated.
        let text = match arg_text(args.into_iter().next()) {
            Some(t) => t,
            None => return Ok(Vec::new().into()),
        };

        // Unparsable JSON -> zero rows, never a panic.
        let root: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new().into()),
        };

        let rows = match which {
            T::Metadata => metadata_rows(&root),
            T::Schema => schema_rows(&root),
            T::Snapshots => snapshot_rows(&root),
        };
        Ok(rows.into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "icebergscan: no pragmas".into(),
        ))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("icebergscan: no casts".into()))
    }
}

export!(Extension);

// ---------------------------------------------------------------------------
// Core readers (pure functions over a parsed serde_json::Value; unit-tested).
// ---------------------------------------------------------------------------

/// iceberg_metadata: top-level table-metadata scalars as (key, value) rows.
/// Only the well-known top-level fields are surfaced; absent fields are skipped.
fn metadata_rows(root: &Value) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::new();
    // The ordered set of top-level keys an inspector cares about.
    const KEYS: &[&str] = &[
        "format-version",
        "table-uuid",
        "location",
        "last-updated-ms",
        "last-column-id",
        "current-schema-id",
        "current-snapshot-id",
    ];
    for &k in KEYS {
        if let Some(v) = root.get(k) {
            // Skip JSON null (treated as absent).
            if v.is_null() {
                continue;
            }
            out.push(vec![
                types::Duckvalue::Text(k.into()),
                types::Duckvalue::Text(scalar_to_string(v).into()),
            ]);
        }
    }
    out
}

/// iceberg_schema: fields of the CURRENT schema -- the schemas[] entry whose
/// schema-id == current-schema-id (falling back to the first schema, then the
/// top-level "schema" object for v1 metadata). Each field -> one row.
fn schema_rows(root: &Value) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let schema = match current_schema(root) {
        Some(s) => s,
        None => return std::vec::Vec::new(),
    };
    let fields = match schema.get("fields").and_then(Value::as_array) {
        Some(f) => f,
        None => return std::vec::Vec::new(),
    };

    let mut out = std::vec::Vec::with_capacity(fields.len());
    for f in fields {
        let field_id = f
            .get("id")
            .and_then(Value::as_i64)
            .map(types::Duckvalue::Int64)
            .unwrap_or(types::Duckvalue::Null);
        let name = f
            .get("name")
            .and_then(Value::as_str)
            .map(|s| types::Duckvalue::Text(s.into()))
            .unwrap_or(types::Duckvalue::Null);
        // "type" is a primitive string ("long", "string", ...) or a nested
        // struct/list/map object -- render the latter as its JSON text.
        let ty = match f.get("type") {
            Some(Value::String(s)) => types::Duckvalue::Text(s.as_str().into()),
            Some(other) => types::Duckvalue::Text(other.to_string().into()),
            None => types::Duckvalue::Null,
        };
        let required = f
            .get("required")
            .and_then(Value::as_bool)
            .map(types::Duckvalue::Boolean)
            .unwrap_or(types::Duckvalue::Null);
        out.push(vec![field_id, name, ty, required]);
    }
    out
}

/// iceberg_snapshots: one row per snapshots[] entry.
fn snapshot_rows(root: &Value) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let snaps = match root.get("snapshots").and_then(Value::as_array) {
        Some(s) => s,
        None => return std::vec::Vec::new(),
    };
    let mut out = std::vec::Vec::with_capacity(snaps.len());
    for s in snaps {
        let snapshot_id = int_field(s, "snapshot-id");
        // sequence-number is absent in v1 metadata -> NULL.
        let sequence_number = int_field(s, "sequence-number");
        let timestamp_ms = int_field(s, "timestamp-ms");
        let manifest_list = s
            .get("manifest-list")
            .and_then(Value::as_str)
            .map(|m| types::Duckvalue::Text(m.into()))
            .unwrap_or(types::Duckvalue::Null);
        out.push(vec![snapshot_id, sequence_number, timestamp_ms, manifest_list]);
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Pick the current schema object: schemas[] matching current-schema-id, else
/// the first of schemas[], else the v1 top-level "schema" object.
fn current_schema(root: &Value) -> Option<&Value> {
    if let Some(schemas) = root.get("schemas").and_then(Value::as_array) {
        if let Some(cur) = root.get("current-schema-id").and_then(Value::as_i64) {
            if let Some(s) = schemas
                .iter()
                .find(|s| s.get("schema-id").and_then(Value::as_i64) == Some(cur))
            {
                return Some(s);
            }
        }
        // No current-schema-id match: fall back to the first listed schema.
        if let Some(first) = schemas.first() {
            return Some(first);
        }
    }
    // v1 metadata stores a single top-level "schema" object.
    root.get("schema")
}

/// An i64 field -> Int64, else NULL.
fn int_field(obj: &Value, key: &str) -> types::Duckvalue {
    obj.get(key)
        .and_then(Value::as_i64)
        .map(types::Duckvalue::Int64)
        .unwrap_or(types::Duckvalue::Null)
}

/// Render a JSON scalar as a string without surrounding quotes (strings) and
/// without trailing decoration (numbers/bools). Non-scalars -> their JSON text.
fn scalar_to_string(v: &Value) -> std::string::String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Pull the single table-fn argument out as text. Accepts a real TEXT value;
/// a BLOB is decoded as UTF-8 (so a metadata.json BLOB also works). Anything
/// else (NULL / absent / non-UTF-8) -> None (caller emits zero rows).
fn arg_text(arg: Option<types::Duckvalue>) -> Option<std::string::String> {
    match arg {
        Some(types::Duckvalue::Text(s)) => Some(s.into()),
        Some(types::Duckvalue::Blob(b)) => {
            let bytes: std::vec::Vec<u8> = b.into();
            std::string::String::from_utf8(bytes).ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Registration.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum T {
    Metadata,
    Schema,
    Snapshots,
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

    // Every fn takes the metadata.json as a single VARCHAR `metadata_json` arg.
    let json_arg = || {
        vec![runtime::Funcarg {
            name: Some("metadata_json".into()),
            logical: types::Logicaltype::Text,
        }]
    };

    // iceberg_metadata ---------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Metadata);
        let columns = vec![
            types::Columndef {
                name: "key".into(),
                logical: types::Logicaltype::Text,
            },
            types::Columndef {
                name: "value".into(),
                logical: types::Logicaltype::Text,
            },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Top-level Iceberg table-metadata fields (format-version, table-uuid, \
                 location, current-snapshot-id, ...) as (key, value) rows"
                    .into(),
            ),
            tags: vec!["iceberg".into(), "metadata".into()],
        };
        reg.register(
            "iceberg_metadata",
            &json_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // iceberg_schema -----------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Schema);
        let columns = vec![
            types::Columndef {
                name: "field_id".into(),
                logical: types::Logicaltype::Int64,
            },
            types::Columndef {
                name: "name".into(),
                logical: types::Logicaltype::Text,
            },
            types::Columndef {
                name: "type".into(),
                logical: types::Logicaltype::Text,
            },
            types::Columndef {
                name: "required".into(),
                logical: types::Logicaltype::Boolean,
            },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Fields of the current Iceberg schema (schemas[] matching \
                 current-schema-id): (field_id, name, type, required) per field"
                    .into(),
            ),
            tags: vec!["iceberg".into(), "schema".into()],
        };
        reg.register(
            "iceberg_schema",
            &json_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // iceberg_snapshots --------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Snapshots);
        let columns = vec![
            types::Columndef {
                name: "snapshot_id".into(),
                logical: types::Logicaltype::Int64,
            },
            types::Columndef {
                name: "sequence_number".into(),
                logical: types::Logicaltype::Int64,
            },
            types::Columndef {
                name: "timestamp_ms".into(),
                logical: types::Logicaltype::Int64,
            },
            types::Columndef {
                name: "manifest_list".into(),
                logical: types::Logicaltype::Text,
            },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Iceberg snapshots[]: (snapshot_id, sequence_number, timestamp_ms, \
                 manifest_list) per snapshot"
                    .into(),
            ),
            tags: vec!["iceberg".into(), "snapshots".into()],
        };
        reg.register(
            "iceberg_snapshots",
            &json_arg(),
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native tests: drive the readers over a tiny inline Iceberg v2 metadata.json.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // A minimal but realistic Iceberg v2 metadata.json: a schema with 2 fields
    // (id long required, data string optional) and one snapshot.
    const META: &str = r#"
    {
      "format-version": 2,
      "table-uuid": "9c12d441-03fe-4693-9a96-a0705ddf69c1",
      "location": "s3://bucket/warehouse/db/table",
      "last-updated-ms": 1602638573874,
      "last-column-id": 2,
      "current-schema-id": 0,
      "schemas": [
        {
          "type": "struct",
          "schema-id": 0,
          "fields": [
            {"id": 1, "name": "id",   "required": true,  "type": "long"},
            {"id": 2, "name": "data", "required": false, "type": "string"}
          ]
        }
      ],
      "current-snapshot-id": 3055729675574597004,
      "snapshots": [
        {
          "snapshot-id": 3055729675574597004,
          "sequence-number": 1,
          "timestamp-ms": 1602638573874,
          "manifest-list": "s3://bucket/warehouse/db/table/metadata/snap-3055729675574597004.avro"
        }
      ]
    }
    "#;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    // Pull the Text out of a Duckvalue for assertions.
    fn text(v: &types::Duckvalue) -> std::string::String {
        match v {
            types::Duckvalue::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn metadata_surfaces_top_level_scalars() {
        let rows = metadata_rows(&parse(META));
        let map: HashMap<std::string::String, std::string::String> = rows
            .iter()
            .map(|r| (text(&r[0]), text(&r[1])))
            .collect();
        assert_eq!(map.get("format-version").map(|s| s.as_str()), Some("2"));
        assert_eq!(
            map.get("table-uuid").map(|s| s.as_str()),
            Some("9c12d441-03fe-4693-9a96-a0705ddf69c1")
        );
        assert_eq!(
            map.get("current-snapshot-id").map(|s| s.as_str()),
            Some("3055729675574597004")
        );
        assert_eq!(
            map.get("location").map(|s| s.as_str()),
            Some("s3://bucket/warehouse/db/table")
        );
    }

    #[test]
    fn schema_emits_two_fields() {
        let rows = schema_rows(&parse(META));
        assert_eq!(rows.len(), 2);
        // field 1: id long required
        assert!(matches!(rows[0][0], types::Duckvalue::Int64(1)));
        assert_eq!(text(&rows[0][1]), "id");
        assert_eq!(text(&rows[0][2]), "long");
        assert!(matches!(rows[0][3], types::Duckvalue::Boolean(true)));
        // field 2: data string optional
        assert!(matches!(rows[1][0], types::Duckvalue::Int64(2)));
        assert_eq!(text(&rows[1][1]), "data");
        assert_eq!(text(&rows[1][2]), "string");
        assert!(matches!(rows[1][3], types::Duckvalue::Boolean(false)));
    }

    #[test]
    fn snapshots_emits_one_snapshot() {
        let rows = snapshot_rows(&parse(META));
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0][0],
            types::Duckvalue::Int64(3055729675574597004)
        ));
        assert!(matches!(rows[0][1], types::Duckvalue::Int64(1)));
        assert!(matches!(rows[0][2], types::Duckvalue::Int64(1602638573874)));
        assert!(text(&rows[0][3]).ends_with("snap-3055729675574597004.avro"));
    }

    #[test]
    fn invalid_json_yields_no_rows() {
        // serde_json::from_str fails -> callers emit zero rows; here we assert the
        // readers themselves never panic on an empty / non-table object.
        let empty = parse("{}");
        assert!(metadata_rows(&empty).is_empty());
        assert!(schema_rows(&empty).is_empty());
        assert!(snapshot_rows(&empty).is_empty());
    }

    #[test]
    fn v1_schema_fallback() {
        // v1 metadata: single top-level "schema" object, no schemas[].
        let v1 = parse(
            r#"{"format-version":1,"schema":{"fields":[{"id":1,"name":"x","required":true,"type":"int"}]}}"#,
        );
        let rows = schema_rows(&v1);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0][1]), "x");
        assert_eq!(text(&rows[0][2]), "int");
    }
}
