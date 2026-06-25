//! Inspect a Delta Lake table's transaction log as DuckDB table functions over
//! an in-memory text/BLOB argument.
//!
//!   delta_log_info(log_json VARCHAR) -> table(
//!       version BIGINT,   -- commit ordinal the action belongs to (best effort)
//!       action  VARCHAR,  -- protocol | metaData | add | remove | <other>
//!       path    VARCHAR,  -- add/remove file path (NULL otherwise)
//!       size    BIGINT)   -- add file size in bytes (NULL otherwise)
//!
//!   delta_schema(log_json VARCHAR) -> table(
//!       column_name VARCHAR,
//!       column_type VARCHAR)  -- the Delta type name (integer, string, ...)
//!
//! A Delta table is a directory: _delta_log/*.json holds the transaction log as
//! JSON-lines (one action per line); *.parquet holds the data. A WIT component
//! cannot list a directory, so the concatenated _delta_log is supplied as the
//! argument. We parse the log only -- the active file list + version come from
//! the `add`/`remove` actions, the schema from the `metaData` action's
//! `schemaString`. Reading the referenced parquet DATA is out of scope.
//!
//! Robustness: a NULL / non-text / unparsable argument yields zero rows, and a
//! line that is not valid JSON (or not a JSON object) is skipped -- never a
//! panic.
//!
//! On `version`: Delta encodes a commit's version in the log FILE name (e.g.
//! 00000000000000000000.json), which is lost when the log is concatenated into
//! one argument. We approximate it by a commit ordinal that advances on each
//! `commitInfo` action (the conventional first line of a commit). With one
//! concatenated commit this is 0 for every action; callers who need the true
//! version should supply one commit's log at a time.
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use serde_json::Value;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_table_fns()?;
        Ok(types::Loadresult {
            name: "deltascan".into(),
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
        Err(types::Duckerror::Unsupported("deltascan: no scalar fns".into()))
    }
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("deltascan: no scalar fns".into()))
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

        // The single argument is the concatenated _delta_log text. Accept TEXT
        // and BLOB (utf-8); a NULL / non-text / non-utf-8 argument -> zero rows.
        let log: std::string::String = match args.into_iter().next() {
            Some(types::Duckvalue::Text(s)) => s.into(),
            Some(types::Duckvalue::Blob(b)) => match std::string::String::from_utf8(b.into()) {
                Ok(s) => s,
                Err(_) => return Ok(Vec::new().into()),
            },
            _ => return Ok(Vec::new().into()),
        };

        let rows = match which {
            T::LogInfo => log_info_rows(&log),
            T::Schema => schema_rows(&log),
        };
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("deltascan: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("deltascan: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("deltascan: no casts".into()))
    }
}

export!(Extension);

// ---- pure parsing (host-testable; takes &str, returns plain Rust) ----------

/// One parsed action row: (version, action, path, size).
type Cell = types::Duckvalue;

/// Parse the concatenated _delta_log JSON-lines and emit one row per action.
/// Each non-empty line is one JSON object with a single top-level key naming the
/// action (protocol / metaData / add / remove / commitInfo / txn / ...). For
/// `add` we surface {path, size}; for `remove` we surface {path}; other actions
/// have NULL path/size. Lines that are blank, not valid JSON, or not a JSON
/// object are skipped (never a panic).
fn log_info_rows(log: &str) -> std::vec::Vec<std::vec::Vec<Cell>> {
    let mut out = std::vec::Vec::new();
    let mut version: i64 = 0;
    let mut seen_commit = false;

    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(m)) => m,
            // a non-object line (or invalid JSON) is not a Delta action
            _ => continue,
        };

        // A commitInfo line conventionally opens a commit; every action after it
        // (and the commitInfo itself) belongs to that commit's version. The first
        // commit stays at 0; each subsequent commitInfo advances the ordinal.
        if obj.contains_key("commitInfo") {
            if seen_commit {
                version += 1;
            }
            seen_commit = true;
        }

        for (action, body) in &obj {
            let (path, size) = match action.as_str() {
                "add" => (json_str(body, "path"), json_i64(body, "size")),
                "remove" => (json_str(body, "path"), json_i64(body, "size")),
                _ => (None, None),
            };
            out.push(vec![
                Cell::Int64(version),
                Cell::Text(action.clone().into()),
                opt_text(path),
                opt_i64(size),
            ]);
        }
    }

    out
}

/// Parse the `metaData` action's `schemaString` (a JSON-encoded Delta struct
/// schema) into (column_name, column_type) rows. The schemaString is itself a
/// JSON string whose decoded value is
///   {"type":"struct","fields":[{"name":..,"type":..,"nullable":..},..]}.
/// A field's `type` may be a primitive name ("integer","string",..) or a nested
/// object (struct/array/map) -- in the nested case we emit its "type" tag
/// (e.g. "struct", "array", "map") rather than recursing. The FIRST metaData
/// action found wins. No metaData / unparsable schema -> zero rows.
fn schema_rows(log: &str) -> std::vec::Vec<std::vec::Vec<Cell>> {
    let mut out = std::vec::Vec::new();

    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(m)) => m,
            _ => continue,
        };
        let Some(meta) = obj.get("metaData") else {
            continue;
        };
        // schemaString is a string field that itself holds JSON.
        let Some(schema_str) = meta.get("schemaString").and_then(Value::as_str) else {
            continue;
        };
        let Ok(schema) = serde_json::from_str::<Value>(schema_str) else {
            // malformed inner schema -> no columns (but stop: metaData found)
            return out;
        };
        let Some(fields) = schema.get("fields").and_then(Value::as_array) else {
            return out;
        };
        for f in fields {
            let Some(name) = f.get("name").and_then(Value::as_str) else {
                continue;
            };
            let ty = match f.get("type") {
                Some(Value::String(s)) => s.clone(),
                // nested type: report the kind tag (struct / array / map)
                Some(Value::Object(m)) => m
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("struct")
                    .to_string(),
                _ => continue,
            };
            out.push(vec![
                Cell::Text(name.to_string().into()),
                Cell::Text(ty.into()),
            ]);
        }
        // first metaData wins
        return out;
    }

    out
}

/// Read a string field of a JSON object; None if absent or not a string.
fn json_str(v: &Value, key: &str) -> Option<std::string::String> {
    v.get(key).and_then(Value::as_str).map(|s| s.to_string())
}

/// Read an integer-valued field of a JSON object; None if absent or not an int.
fn json_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn opt_text(s: Option<std::string::String>) -> Cell {
    match s {
        Some(s) => Cell::Text(s.into()),
        None => Cell::Null,
    }
}

fn opt_i64(n: Option<i64>) -> Cell {
    match n {
        Some(n) => Cell::Int64(n),
        None => Cell::Null,
    }
}

// ---- registration ----------------------------------------------------------

fn register_table_fns() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    // delta_log_info(log_json) -> (version, action, path, size)
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::LogInfo);
        let args = vec![runtime::Funcarg {
            name: Some("log_json".into()),
            logical: types::Logicaltype::Text,
        }];
        let columns = vec![
            types::Columndef {
                name: "version".into(),
                logical: types::Logicaltype::Int64,
            },
            types::Columndef {
                name: "action".into(),
                logical: types::Logicaltype::Text,
            },
            types::Columndef {
                name: "path".into(),
                logical: types::Logicaltype::Text,
            },
            types::Columndef {
                name: "size".into(),
                logical: types::Logicaltype::Int64,
            },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Parse a Delta Lake _delta_log (concatenated JSON-lines) into one \
                 (version, action, path, size) row per transaction-log action"
                    .into(),
            ),
            tags: vec!["delta".into(), "deltalake".into()],
        };
        reg.register(
            "delta_log_info",
            &args,
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    // delta_schema(log_json) -> (column_name, column_type)
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Schema);
        let args = vec![runtime::Funcarg {
            name: Some("log_json".into()),
            logical: types::Logicaltype::Text,
        }];
        let columns = vec![
            types::Columndef {
                name: "column_name".into(),
                logical: types::Logicaltype::Text,
            },
            types::Columndef {
                name: "column_type".into(),
                logical: types::Logicaltype::Text,
            },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Parse the metaData action's Delta schemaString from a _delta_log \
                 into (column_name, column_type) rows"
                    .into(),
            ),
            tags: vec!["delta".into(), "deltalake".into(), "schema".into()],
        };
        reg.register(
            "delta_schema",
            &args,
            &columns,
            runtime::TableCallback::new(h),
            Some(&opts),
        )?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    LogInfo,
    Schema,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---- native tests (host; exercise the pure parsers only) -------------------

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny _delta_log: protocol, metaData (schema a:int, b:string), one add.
    const LOG: &str = concat!(
        r#"{"protocol":{"minReaderVersion":1,"minWriterVersion":2}}"#,
        "\n",
        r#"{"metaData":{"id":"t","format":{"provider":"parquet"},"schemaString":"{\"type\":\"struct\",\"fields\":[{\"name\":\"a\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}},{\"name\":\"b\",\"type\":\"string\",\"nullable\":true,\"metadata\":{}}]}","partitionColumns":[]}}"#,
        "\n",
        r#"{"add":{"path":"part-0001.parquet","size":1234,"dataChange":true}}"#,
        "\n",
    );

    fn text(c: &Cell) -> Option<&str> {
        match c {
            Cell::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }
    fn i64v(c: &Cell) -> Option<i64> {
        match c {
            Cell::Int64(n) => Some(*n),
            _ => None,
        }
    }
    fn is_null(c: &Cell) -> bool {
        matches!(c, Cell::Null)
    }

    #[test]
    fn log_info_emits_each_action() {
        let rows = log_info_rows(LOG);
        assert_eq!(rows.len(), 3);

        assert_eq!(text(&rows[0][1]), Some("protocol"));
        assert!(is_null(&rows[0][2]));
        assert!(is_null(&rows[0][3]));

        assert_eq!(text(&rows[1][1]), Some("metaData"));

        // the add carries path + size
        assert_eq!(text(&rows[2][1]), Some("add"));
        assert_eq!(text(&rows[2][2]), Some("part-0001.parquet"));
        assert_eq!(i64v(&rows[2][3]), Some(1234));
    }

    #[test]
    fn schema_emits_columns() {
        let rows = schema_rows(LOG);
        assert_eq!(rows.len(), 2);
        assert_eq!(text(&rows[0][0]), Some("a"));
        assert_eq!(text(&rows[0][1]), Some("integer"));
        assert_eq!(text(&rows[1][0]), Some("b"));
        assert_eq!(text(&rows[1][1]), Some("string"));
    }

    #[test]
    fn remove_action_surfaces_path() {
        let log = r#"{"remove":{"path":"old.parquet","deletionTimestamp":1,"dataChange":true}}"#;
        let rows = log_info_rows(log);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0][1]), Some("remove"));
        assert_eq!(text(&rows[0][2]), Some("old.parquet"));
    }

    #[test]
    fn commitinfo_advances_version() {
        let log = concat!(
            r#"{"commitInfo":{"timestamp":1}}"#,
            "\n",
            r#"{"add":{"path":"p0.parquet","size":1}}"#,
            "\n",
            r#"{"commitInfo":{"timestamp":2}}"#,
            "\n",
            r#"{"add":{"path":"p1.parquet","size":2}}"#,
        );
        let rows = log_info_rows(log);
        // commitInfo(v0), add(v0), commitInfo(v1), add(v1)
        assert_eq!(i64v(&rows[0][0]), Some(0));
        assert_eq!(i64v(&rows[1][0]), Some(0));
        assert_eq!(i64v(&rows[2][0]), Some(1));
        assert_eq!(i64v(&rows[3][0]), Some(1));
    }

    #[test]
    fn malformed_and_empty_yield_no_rows_or_skip() {
        // blank / garbage / non-object lines are skipped, never a panic.
        assert!(log_info_rows("").is_empty());
        assert!(log_info_rows("not json\n   \n[1,2,3]").is_empty());
        assert!(schema_rows("").is_empty());
        // valid add interleaved with garbage: only the add survives.
        let log = "garbage\n{\"add\":{\"path\":\"p.parquet\",\"size\":7}}\n[]";
        let rows = log_info_rows(log);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0][2]), Some("p.parquet"));
    }

    #[test]
    fn schema_missing_metadata_is_empty() {
        let log = r#"{"add":{"path":"p.parquet","size":1}}"#;
        assert!(schema_rows(log).is_empty());
    }

    #[test]
    fn schema_nested_type_reports_kind() {
        // a field whose type is a nested struct object -> we report "struct".
        let inner = r#"{"type":"struct","fields":[{"name":"c","type":{"type":"array","elementType":"long","containsNull":true},"nullable":true,"metadata":{}}]}"#;
        let escaped = serde_json::to_string(inner).unwrap();
        let log = format!(r#"{{"metaData":{{"schemaString":{}}}}}"#, escaped);
        let rows = schema_rows(&log);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0][0]), Some("c"));
        assert_eq!(text(&rows[0][1]), Some("array"));
    }
}
