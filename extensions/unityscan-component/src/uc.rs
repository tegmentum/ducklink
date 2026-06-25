//! Unity Catalog REST surface: DSN parsing, endpoint URL builders, and the
//! JSON-response parsers. These are pure (no network), so they are exhaustively
//! unit-tested offline against captured-shape UC responses.
//!
//! The REST endpoints mirror the official `unity_catalog` extension
//! (src/uc_api.cpp), which is the open Databricks Unity Catalog REST API:
//!
//!   GET {base}/api/2.1/unity-catalog/catalogs
//!   GET {base}/api/2.1/unity-catalog/schemas?catalog_name={cat}
//!   GET {base}/api/2.1/unity-catalog/tables?catalog_name={cat}&schema_name={sch}
//!
//! `schemas[]` items carry `name`; `tables[]` items carry `name`, `table_type`,
//! `data_source_format`, `storage_location`, `table_id`, and `columns[]`. Each
//! column carries `name`, `type_text`, `type_precision`, `type_scale`,
//! `position`.

use serde_json::Value;

/// Connection config parsed from the ATTACH DSN.
pub struct UcConfig {
    /// Base endpoint, e.g. `https://host` or `https://host/api/...` trimmed to
    /// the scheme+authority. No trailing slash.
    pub endpoint: String,
    /// Bearer token (may be empty for an unauthenticated open `unitycatalog`).
    pub token: String,
    /// The UC catalog name to enumerate (the ATTACH default catalog).
    pub catalog: String,
}

/// A schema returned by the UC `/schemas` endpoint.
#[derive(Debug, PartialEq, Eq)]
pub struct UcSchema {
    pub name: String,
}

/// A column of a UC table.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct UcColumn {
    pub name: String,
    pub type_text: String,
    pub position: u64,
}

/// A table returned by the UC `/tables` endpoint.
#[derive(Debug, PartialEq, Eq)]
pub struct UcTable {
    pub name: String,
    pub schema_name: String,
    pub table_type: String,
    pub data_source_format: String,
    /// Where the table's data files live (s3://, az://, file:// ...). This is
    /// what a real data scan would feed to s3fs/azfs + delta/parquet.
    pub storage_location: String,
    pub table_id: String,
    pub columns: Vec<UcColumn>,
}

/// Parse an ATTACH DSN into a `UcConfig`.
///
/// Accepted forms (the URL host is the UC endpoint; params follow as `;`/space
/// separated `key=value`):
///   `https://host;token=...;catalog=main`
///   `https://host/ ; token=...`         (token / catalog keys, any order)
///   `endpoint=https://host token=... catalog=main`
///
/// `catalog` defaults to "main" (the conventional UC default workspace catalog
/// in the open `unitycatalog`).
pub fn parse_dsn(dsn: &str) -> Result<UcConfig, String> {
    let dsn = dsn.trim();
    let mut endpoint = String::new();
    let mut token = String::new();
    let mut catalog = String::new();

    // Split on ';' (UC convention) and whitespace; the first bare token that
    // looks like a URL becomes the endpoint.
    for raw in dsn.split(|c: char| c == ';' || c.is_whitespace()) {
        let tok = raw.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some((k, v)) = tok.split_once('=') {
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "endpoint" | "url" | "host" => endpoint = v.to_string(),
                "token" | "bearer" | "pat" => token = v.to_string(),
                "catalog" | "catalog_name" => catalog = v.to_string(),
                _ => { /* ignore unknown keys */ }
            }
        } else if endpoint.is_empty()
            && (tok.starts_with("http://") || tok.starts_with("https://"))
        {
            endpoint = tok.to_string();
        }
    }

    if endpoint.is_empty() {
        return Err("unity dsn: missing endpoint (expected an http(s):// URL)".to_string());
    }
    // Normalize: keep scheme + authority, drop any path/trailing slash so the
    // URL builders can append the canonical /api/2.1/... path.
    let endpoint = normalize_endpoint(&endpoint);
    if catalog.is_empty() {
        catalog = "main".to_string();
    }
    Ok(UcConfig {
        endpoint,
        token,
        catalog,
    })
}

/// Trim an endpoint URL to `scheme://authority` (no path, no trailing slash).
fn normalize_endpoint(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https://", r)
    } else if let Some(r) = url.strip_prefix("http://") {
        ("http://", r)
    } else {
        return url.to_string();
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}{authority}")
}

/// Percent-encode a UC name for use in a query string (UC names allow letters,
/// digits, `_`; we still encode anything non-unreserved defensively).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The /catalogs endpoint. Not on the storage hot path (ATTACH names a single
/// catalog), but kept + tested for completeness / future multi-catalog listing.
#[allow(dead_code)]
pub fn catalogs_url(endpoint: &str) -> String {
    format!("{endpoint}/api/2.1/unity-catalog/catalogs")
}

pub fn schemas_url(endpoint: &str, catalog: &str) -> String {
    format!(
        "{endpoint}/api/2.1/unity-catalog/schemas?catalog_name={}",
        enc(catalog)
    )
}

pub fn tables_url(endpoint: &str, catalog: &str, schema: &str) -> String {
    format!(
        "{endpoint}/api/2.1/unity-catalog/tables?catalog_name={}&schema_name={}",
        enc(catalog),
        enc(schema)
    )
}

pub fn table_url(endpoint: &str, full_name: &str) -> String {
    format!(
        "{endpoint}/api/2.1/unity-catalog/tables/{}",
        enc(full_name)
    )
}

/// If the UC JSON carries an `error_code`, surface it as an Err (matches the
/// official extension's CheckError).
fn check_error(root: &Value) -> Result<(), String> {
    if let Some(code) = root.get("error_code").and_then(Value::as_str) {
        if !code.is_empty() {
            let msg = root.get("message").and_then(Value::as_str).unwrap_or("-");
            return Err(format!("UC error_code={code}, message={msg}"));
        }
    }
    Ok(())
}

/// Parse a `/catalogs` response -> the catalog names (root `catalogs[].name`).
#[allow(dead_code)]
pub fn parse_catalogs(json: &str) -> Result<Vec<String>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("json: {e}"))?;
    check_error(&root)?;
    let arr = root
        .get("catalogs")
        .and_then(Value::as_array)
        .ok_or("response missing `catalogs` array")?;
    Ok(arr
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
        .collect())
}

/// Parse a `/schemas` response -> the schemas (root `schemas[].name`).
pub fn parse_schemas(json: &str) -> Result<Vec<UcSchema>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("json: {e}"))?;
    check_error(&root)?;
    let arr = root
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or("response missing `schemas` array")?;
    Ok(arr
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .map(|n| UcSchema { name: n.to_string() })
        .collect())
}

fn parse_columns(table: &Value) -> Vec<UcColumn> {
    table
        .get("columns")
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(|c| {
                    let name = c.get("name").and_then(Value::as_str)?;
                    let type_text = c
                        .get("type_text")
                        .and_then(Value::as_str)
                        .unwrap_or("string");
                    let position = c.get("position").and_then(Value::as_u64).unwrap_or(0);
                    Some(UcColumn {
                        name: name.to_string(),
                        type_text: type_text.to_string(),
                        position,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_one_table(table: &Value, fallback_schema: &str) -> Option<UcTable> {
    let name = table.get("name").and_then(Value::as_str)?;
    let schema_name = table
        .get("schema_name")
        .and_then(Value::as_str)
        .unwrap_or(fallback_schema)
        .to_string();
    Some(UcTable {
        name: name.to_string(),
        schema_name,
        table_type: table
            .get("table_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        data_source_format: table
            .get("data_source_format")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        storage_location: table
            .get("storage_location")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        table_id: table
            .get("table_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        columns: parse_columns(table),
    })
}

/// Parse a `/tables?catalog_name=&schema_name=` response -> the tables
/// (root `tables[]`, each with name/columns/storage_location).
pub fn parse_tables(json: &str, schema: &str) -> Result<Vec<UcTable>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("json: {e}"))?;
    check_error(&root)?;
    let arr = root
        .get("tables")
        .and_then(Value::as_array)
        .ok_or("response missing `tables` array")?;
    Ok(arr
        .iter()
        .filter_map(|t| parse_one_table(t, schema))
        .collect())
}

/// Parse a single-table `/tables/{full_name}` response -> the table (the body
/// IS the table object, not wrapped in an array).
pub fn parse_table(json: &str) -> Result<UcTable, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("json: {e}"))?;
    check_error(&root)?;
    parse_one_table(&root, "").ok_or_else(|| "table response missing `name`".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- DSN parsing --------------------------------------------------------

    #[test]
    fn dsn_url_then_keys() {
        let c = parse_dsn("https://uc.example.com ; token=abc123 ; catalog=sales").unwrap();
        assert_eq!(c.endpoint, "https://uc.example.com");
        assert_eq!(c.token, "abc123");
        assert_eq!(c.catalog, "sales");
    }

    #[test]
    fn dsn_keyvalue_only() {
        let c = parse_dsn("endpoint=http://localhost:8080 token=tok catalog=unity").unwrap();
        assert_eq!(c.endpoint, "http://localhost:8080");
        assert_eq!(c.token, "tok");
        assert_eq!(c.catalog, "unity");
    }

    #[test]
    fn dsn_defaults_catalog_main() {
        let c = parse_dsn("https://host/api/2.1/unity-catalog/").unwrap();
        assert_eq!(c.endpoint, "https://host"); // path stripped
        assert_eq!(c.catalog, "main");
        assert_eq!(c.token, "");
    }

    #[test]
    fn dsn_missing_endpoint_errs() {
        assert!(parse_dsn("token=abc catalog=main").is_err());
    }

    // ---- URL builders -------------------------------------------------------

    #[test]
    fn urls_match_official_paths() {
        let ep = "https://uc.example.com";
        assert_eq!(
            catalogs_url(ep),
            "https://uc.example.com/api/2.1/unity-catalog/catalogs"
        );
        assert_eq!(
            schemas_url(ep, "main"),
            "https://uc.example.com/api/2.1/unity-catalog/schemas?catalog_name=main"
        );
        assert_eq!(
            tables_url(ep, "main", "default"),
            "https://uc.example.com/api/2.1/unity-catalog/tables?catalog_name=main&schema_name=default"
        );
        assert_eq!(
            table_url(ep, "main.default.t"),
            "https://uc.example.com/api/2.1/unity-catalog/tables/main.default.t"
        );
    }

    // ---- JSON parsing (the core deliverable) --------------------------------

    #[test]
    fn parse_catalogs_extracts_names() {
        let json = r#"{
          "catalogs": [
            {"name": "main", "comment": "default"},
            {"name": "sales"}
          ]
        }"#;
        assert_eq!(parse_catalogs(json).unwrap(), vec!["main", "sales"]);
    }

    #[test]
    fn parse_schemas_extracts_names() {
        let json = r#"{
          "schemas": [
            {"name": "default", "catalog_name": "main"},
            {"name": "bronze",  "catalog_name": "main"}
          ]
        }"#;
        let s = parse_schemas(json).unwrap();
        assert_eq!(
            s,
            vec![
                UcSchema { name: "default".into() },
                UcSchema { name: "bronze".into() },
            ]
        );
    }

    /// A captured-shape UC /tables response: a delta table on s3 with three
    /// typed columns. Asserts table name, storage_location, and every column's
    /// name + type_text + position parse correctly.
    #[test]
    fn parse_tables_extracts_table_columns_and_location() {
        let json = r#"{
          "tables": [
            {
              "name": "trips",
              "catalog_name": "main",
              "schema_name": "default",
              "table_type": "EXTERNAL",
              "data_source_format": "DELTA",
              "storage_location": "s3://my-bucket/main/default/trips",
              "table_id": "abcd-1234",
              "columns": [
                {"name": "id",      "type_text": "bigint", "type_precision": 0, "type_scale": 0, "position": 0},
                {"name": "fare",    "type_text": "double", "type_precision": 0, "type_scale": 0, "position": 1},
                {"name": "rider",   "type_text": "string", "type_precision": 0, "type_scale": 0, "position": 2}
              ],
              "properties": {"delta.minReaderVersion": "1"}
            }
          ]
        }"#;
        let tables = parse_tables(json, "default").unwrap();
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.name, "trips");
        assert_eq!(t.schema_name, "default");
        assert_eq!(t.table_type, "EXTERNAL");
        assert_eq!(t.data_source_format, "DELTA");
        assert_eq!(t.storage_location, "s3://my-bucket/main/default/trips");
        assert_eq!(t.table_id, "abcd-1234");
        assert_eq!(t.columns.len(), 3);
        assert_eq!(
            t.columns,
            vec![
                UcColumn { name: "id".into(),    type_text: "bigint".into(), position: 0 },
                UcColumn { name: "fare".into(),  type_text: "double".into(), position: 1 },
                UcColumn { name: "rider".into(), type_text: "string".into(), position: 2 },
            ]
        );
    }

    #[test]
    fn parse_table_single_object() {
        let json = r#"{
          "name": "events",
          "catalog_name": "main",
          "schema_name": "silver",
          "table_type": "MANAGED",
          "data_source_format": "DELTA",
          "storage_location": "az://container/main/silver/events",
          "table_id": "ev-99",
          "columns": [
            {"name": "ts",  "type_text": "timestamp", "position": 0},
            {"name": "msg", "type_text": "string",    "position": 1}
          ]
        }"#;
        let t = parse_table(json).unwrap();
        assert_eq!(t.name, "events");
        assert_eq!(t.schema_name, "silver");
        assert_eq!(t.storage_location, "az://container/main/silver/events");
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[0].name, "ts");
        assert_eq!(t.columns[0].type_text, "timestamp");
        assert_eq!(t.columns[1].name, "msg");
    }

    #[test]
    fn parse_surfaces_uc_error_code() {
        let json = r#"{"error_code":"PERMISSION_DENIED","message":"no access to catalog"}"#;
        let err = parse_schemas(json).unwrap_err();
        assert!(err.contains("PERMISSION_DENIED"));
        assert!(err.contains("no access"));
    }

    #[test]
    fn parse_empty_arrays() {
        assert_eq!(parse_schemas(r#"{"schemas":[]}"#).unwrap(), vec![]);
        assert_eq!(
            parse_tables(r#"{"tables":[]}"#, "default").unwrap(),
            Vec::<UcTable>::new()
        );
    }

    #[test]
    fn parse_malformed_json_errs() {
        assert!(parse_schemas("not json").is_err());
        assert!(parse_tables("{", "s").is_err());
    }
}
