//! Pure JSON logic, host-buildable and unit-tested. No wit / no wasm here.
//!
//! Every function returns Option / bool and NEVER panics: invalid JSON, bad
//! paths, or missing values map to `None` (-> SQL NULL) just like DuckDB.
//!
//! Path resolution accepts two syntaxes, matching DuckDB's json extension:
//!   - `$`-style JSONPath: `$`, `$.a.b`, `$[0]`, `$.a[0]`  (via serde_json_path)
//!   - JSONPointer:        `/a/b`, `/a/0`                  (serde_json built-in)

use serde_json::Value;
use serde_json_path::JsonPath;

/// Resolve a path against a parsed value, returning the single matched node.
///
/// A bare `$` returns the root. `$`-prefixed paths go through serde_json_path;
/// `/`-prefixed paths go through serde_json's JSONPointer. We return the first
/// match only (DuckDB's single-path extract is scalar-valued).
fn resolve<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let p = path.trim();
    if p == "$" || p.is_empty() {
        return Some(root);
    }
    if let Some(stripped) = p.strip_prefix('/') {
        // JSONPointer wants a leading '/'; pass the original.
        let _ = stripped;
        return root.pointer(p);
    }
    if p.starts_with('$') {
        let jp = JsonPath::parse(p).ok()?;
        return jp.query(root).first();
    }
    // Bare key path like "a.b" — treat as a relaxed $-path by prefixing.
    let jp = JsonPath::parse(&format!("$.{p}")).ok()?;
    jp.query(root).first()
}

fn parse(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text).ok()
}

/// json_valid(text) -> BOOLEAN
pub fn json_valid(text: &str) -> bool {
    parse(text).is_some()
}

/// json_extract(text, path) -> VARCHAR (JSON text at the path; quotes kept).
pub fn json_extract(text: &str, path: &str) -> Option<String> {
    let root = parse(text)?;
    let node = resolve(&root, path)?;
    Some(node.to_string())
}

/// json_extract_string(text, path) -> VARCHAR (unquoted string).
///
/// For JSON strings the surrounding quotes are removed and escapes decoded.
/// For non-string scalars/containers the JSON text is returned. A JSON `null`
/// at the path yields SQL NULL (matches DuckDB).
pub fn json_extract_string(text: &str, path: &str) -> Option<String> {
    let root = parse(text)?;
    let node = resolve(&root, path)?;
    match node {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// json_array_length(text [, path]) -> BIGINT. NULL if the target isn't an array.
pub fn json_array_length(text: &str, path: Option<&str>) -> Option<i64> {
    let root = parse(text)?;
    let node = match path {
        Some(p) => resolve(&root, p)?,
        None => &root,
    };
    match node {
        Value::Array(a) => Some(a.len() as i64),
        _ => None,
    }
}

/// json_type(text [, path]) -> VARCHAR. Uses DuckDB's logical-type names.
pub fn json_type(text: &str, path: Option<&str>) -> Option<String> {
    let root = parse(text)?;
    let node = match path {
        Some(p) => resolve(&root, p)?,
        None => &root,
    };
    Some(type_name(node).to_string())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Bool(_) => "BOOLEAN",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "BIGINT"
            } else {
                "DOUBLE"
            }
        }
        Value::String(_) => "VARCHAR",
        Value::Array(_) => "ARRAY",
        Value::Object(_) => "OBJECT",
    }
}

/// json_keys(text [, path]) -> VARCHAR (a JSON array of the object's keys).
/// NULL if the target isn't an object.
pub fn json_keys(text: &str, path: Option<&str>) -> Option<String> {
    let root = parse(text)?;
    let node = match path {
        Some(p) => resolve(&root, p)?,
        None => &root,
    };
    match node {
        Value::Object(map) => {
            let keys: Vec<Value> = map.keys().map(|k| Value::String(k.clone())).collect();
            Some(Value::Array(keys).to_string())
        }
        _ => None,
    }
}

/// json_contains(haystack, needle) -> BOOLEAN.
///
/// DuckDB semantics: an object contains another if every key/value of the
/// needle is present (recursively); an array contains a value if any element
/// equals (deep-equals) the needle. Invalid JSON on either side -> NULL.
pub fn json_contains(haystack: &str, needle: &str) -> Option<bool> {
    let hay = parse(haystack)?;
    let need = parse(needle)?;
    Some(contains(&hay, &need))
}

fn contains(hay: &Value, need: &Value) -> bool {
    match (hay, need) {
        (Value::Object(h), Value::Object(n)) => n
            .iter()
            .all(|(k, nv)| h.get(k).map(|hv| contains(hv, nv)).unwrap_or(false)),
        (Value::Array(h), Value::Array(n)) => {
            // every needle element must be contained in the haystack array
            n.iter().all(|nv| h.iter().any(|hv| contains(hv, nv)))
        }
        (Value::Array(h), _) => h.iter().any(|hv| hv == need),
        _ => hay == need,
    }
}

/// json_quote(text) / to_json(text): wrap an arbitrary string as a JSON string.
/// Always succeeds. (DuckDB's to_json is type-aware; here the wasm arg arrives
/// as VARCHAR, so we json-encode the string.)
pub fn json_quote(text: &str) -> String {
    Value::String(text.to_string()).to_string()
}
