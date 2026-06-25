//! Function prefixes — SPARQL-style `prefix__name` namespacing for DuckDB
//! functions (see docs/plans/PLAN-prefixes.md).
//!
//! Every component scalar/table/aggregate registration is forwarded to the core
//! TWICE: once under its bare `name` (current behavior) and once under the
//! qualified `{prefix}__{name}`. The qualified form is always unique, so it
//! never conflicts; the bare form keeps DuckDB's last-registered-wins semantics
//! (confirmed empirically: same name+signature MERGES/replaces with no error,
//! different signatures coexist as overloads).
//!
//! The prefix + expansion for an extension come from its registry/index.json
//! entry's `prefix`/`expansion` fields. If absent (the v1 deprecation window),
//! the prefix falls back to the extension name and the expansion to
//! `ducklink-internal://<extension>`, with a one-time load warning.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The prefix + expansion an extension uses to namespace its functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixInfo {
    /// Short prefix used in SQL (`jsonfns` in `jsonfns__json_valid`).
    pub prefix: String,
    /// Opaque global-identity token (a URL, URN, reverse-DNS string, …).
    pub expansion: String,
    /// True when both fields were absent from the registry and we synthesized
    /// the deprecation fallback — the host warns once per extension.
    pub is_fallback: bool,
}

/// name -> {prefix, expansion} loaded once from registry/index.json at host
/// start. Unknown extensions resolve to the deprecation fallback.
#[derive(Default, Debug)]
pub struct PrefixRegistry {
    entries: HashMap<String, (String, String)>,
    /// Extensions we've already emitted the fallback warning for.
    warned_fallback: std::cell::RefCell<HashSet<String>>,
}

impl PrefixRegistry {
    /// Load the registry/index.json `extensions[]` array into a name ->
    /// (prefix, expansion) map. Entries missing either field are simply not
    /// inserted (they resolve to the fallback at lookup time). A missing or
    /// unparseable file yields an empty registry (everything falls back).
    pub fn load_from_index(path: &Path) -> Self {
        let mut entries = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(exts) = json.get("extensions").and_then(|v| v.as_array()) {
                    for ext in exts {
                        let name = ext.get("name").and_then(|v| v.as_str());
                        let prefix = ext.get("prefix").and_then(|v| v.as_str());
                        let expansion = ext.get("expansion").and_then(|v| v.as_str());
                        if let (Some(name), Some(prefix), Some(expansion)) =
                            (name, prefix, expansion)
                        {
                            entries.insert(
                                name.to_string(),
                                (prefix.to_string(), expansion.to_string()),
                            );
                        }
                    }
                }
            }
        }
        Self {
            entries,
            warned_fallback: std::cell::RefCell::new(HashSet::new()),
        }
    }

    /// Resolve the prefix info for an extension. Returns the registry entry when
    /// present; otherwise the deprecation fallback (prefix = extension name,
    /// expansion = `ducklink-internal://<extension>`), warning once to stderr.
    pub fn resolve(&self, extension: &str) -> PrefixInfo {
        if let Some((prefix, expansion)) = self.entries.get(extension) {
            return PrefixInfo {
                prefix: sanitize_prefix(prefix).unwrap_or_else(|| sanitize_name(extension)),
                expansion: expansion.clone(),
                is_fallback: false,
            };
        }
        if self.warned_fallback.borrow_mut().insert(extension.to_string()) {
            eprintln!(
                "[prefix] WARNING: extension '{extension}' has no prefix/expansion in \
                 registry/index.json; using deprecation fallback prefix='{extension}', \
                 expansion='ducklink-internal://{extension}'. This becomes a hard error \
                 after ducklink v1.1 — add `prefix` and `expansion` to its registry entry."
            );
        }
        PrefixInfo {
            prefix: sanitize_name(extension),
            expansion: format!("ducklink-internal://{extension}"),
            is_fallback: true,
        }
    }
}

/// Build the qualified name `{prefix}__{name}` for a function, or `None` if it
/// should be skipped:
///   * the bare name already contains `__` (likely already prefixed — avoid
///     double-prefixing `jsonfns__json_valid` into `jsonfns__jsonfns__…`),
///   * the prefix is empty after sanitization.
pub fn qualified_name(prefix: &str, bare_name: &str) -> Option<String> {
    let prefix = sanitize_prefix(prefix)?;
    if bare_name.contains("__") {
        return None;
    }
    Some(format!("{prefix}__{bare_name}"))
}

/// A prefix must be a valid unquoted DuckDB identifier head:
/// `[A-Za-z_][A-Za-z0-9_]*`. Returns the prefix unchanged when valid, else
/// `None`. (Hyphens in extension names like `iban-component` are NOT valid; the
/// caller uses `sanitize_name` for the fallback so they become `iban_component`.)
pub fn sanitize_prefix(prefix: &str) -> Option<String> {
    let mut chars = prefix.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    if prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(prefix.to_string())
    } else {
        None
    }
}

/// Coerce an arbitrary extension name into a valid identifier prefix by
/// replacing every disallowed character with `_` (and prefixing `_` if it would
/// start with a digit). Used for the deprecation fallback only.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, ch) in raw.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
        if i == 0 && out.starts_with(|c: char| c.is_ascii_digit()) {
            out.insert(0, '_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// The shape (kind) of a function registration, used as a collision key
/// component and recorded in `__ducklink_prefix_function.shape`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    Scalar,
    Table,
    Aggregate,
}

impl Shape {
    pub fn as_str(self) -> &'static str {
        match self {
            Shape::Scalar => "scalar",
            Shape::Table => "table",
            Shape::Aggregate => "aggregate",
        }
    }
}

/// One registration's identity for collision detection: the bare name, shape,
/// and arity. (DuckDB distinguishes overloads by signature; we approximate with
/// arity, matching the plan's `(name, shape, n_args)` key.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CollisionKey {
    name: String,
    shape: Shape,
    n_args: i32,
}

/// Tracks which expansions have registered each `(name, shape, n_args)` so the
/// host can warn on the 2nd+ registration from a DIFFERENT expansion.
#[derive(Default, Debug)]
pub struct CollisionTracker {
    // key -> ordered list of (extension, expansion); last entry is the current
    // bare owner (DuckDB last-wins).
    seen: HashMap<CollisionKey, Vec<(String, String)>>,
}

/// The outcome of recording one registration. When a collision is detected the
/// host emits a load-time warning built from these fields.
pub struct CollisionReport {
    pub is_collision: bool,
    pub bare_name: String,
    pub shape: Shape,
    pub n_args: i32,
    /// All extensions that have registered this (name, shape, arity), in load
    /// order; the last is the current bare owner.
    pub colliding_extensions: Vec<String>,
    /// The qualified forms available, one per distinct (extension) registration.
    pub qualified_forms: Vec<String>,
}

impl CollisionTracker {
    /// Record a registration; returns a report. `is_collision` is true when a
    /// prior registration of the same (name, shape, n_args) exists from a
    /// DIFFERENT expansion.
    pub fn record(
        &mut self,
        extension: &str,
        expansion: &str,
        info: &PrefixInfo,
        bare_name: &str,
        shape: Shape,
        n_args: i32,
    ) -> CollisionReport {
        let key = CollisionKey {
            name: bare_name.to_string(),
            shape,
            n_args,
        };
        let entry = self.seen.entry(key).or_default();
        let is_collision = entry
            .iter()
            .any(|(_, prior_expansion)| prior_expansion != expansion);
        // Avoid duplicating an identical (extension, expansion) pair (e.g. a
        // re-load) so the warning lists each colliding extension once.
        if !entry
            .iter()
            .any(|(e, x)| e == extension && x == expansion)
        {
            entry.push((extension.to_string(), expansion.to_string()));
        }
        let colliding_extensions: Vec<String> =
            entry.iter().map(|(e, _)| e.clone()).collect();
        // Build the qualified form for each colliding extension. We only know
        // the CURRENT registration's prefix; others reuse the recorded
        // extension name as a best-effort prefix (the qualified form for prior
        // extensions follows the same prefix rule, but the tracker doesn't keep
        // their PrefixInfo, so we synthesize from the extension name — adequate
        // for the warning message).
        let qualified_forms: Vec<String> = entry
            .iter()
            .map(|(e, _)| {
                if e == extension {
                    qualified_name(&info.prefix, bare_name)
                        .unwrap_or_else(|| format!("{}__{bare_name}", info.prefix))
                } else {
                    let p = sanitize_name(e);
                    format!("{p}__{bare_name}")
                }
            })
            .collect();
        CollisionReport {
            is_collision,
            bare_name: bare_name.to_string(),
            shape,
            n_args,
            colliding_extensions,
            qualified_forms,
        }
    }
}

/// Format the load-time collision warning (the plan's Case 2 message).
pub fn format_collision_warning(report: &CollisionReport) -> String {
    let owner = report
        .colliding_extensions
        .last()
        .map(String::as_str)
        .unwrap_or("?");
    format!(
        "[prefix] WARNING: bare name '{name}' ({shape}/{n_args}-arg) is registered by \
         multiple extensions [{exts}]; bare '{name}(...)' now resolves to '{owner}' \
         (last loaded). Use a qualified form to disambiguate: {quals}.",
        name = report.bare_name,
        shape = report.shape.as_str(),
        n_args = report.n_args,
        exts = report.colliding_extensions.join(", "),
        owner = owner,
        quals = report.qualified_forms.join(", "),
    )
}

/// One staged row destined for the `__ducklink_prefix*` tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixRow {
    pub prefix: String,
    pub expansion: String,
    pub extension: String,
    pub function_name: String,
    pub shape: &'static str,
    pub n_args: i32,
}

/// The `__ducklink_prefix*` schema DDL (idempotent). Created on first prefixed
/// registration flush or first `.prefix` use.
pub const PREFIX_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS __ducklink_prefix (\
  name VARCHAR PRIMARY KEY,\
  expansion VARCHAR NOT NULL,\
  description VARCHAR,\
  created_at BIGINT NOT NULL,\
  last_used_at BIGINT\
);\
CREATE TABLE IF NOT EXISTS __ducklink_prefix_function (\
  expansion VARCHAR NOT NULL,\
  function_name VARCHAR NOT NULL,\
  extension_name VARCHAR,\
  shape VARCHAR NOT NULL,\
  n_args INTEGER NOT NULL,\
  registered_at BIGINT NOT NULL,\
  PRIMARY KEY (expansion, function_name, shape, n_args)\
);\
CREATE TABLE IF NOT EXISTS __ducklink_prefix_pin (\
  function_name VARCHAR NOT NULL,\
  shape VARCHAR NOT NULL,\
  n_args INTEGER NOT NULL,\
  expansion VARCHAR NOT NULL,\
  set_at BIGINT NOT NULL,\
  PRIMARY KEY (function_name, shape, n_args)\
);";

/// Escape a string literal for inlining into SQL (double single-quotes).
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Build the full SQL script that ensures the schema exists and upserts the
/// staged prefix + function rows. The timestamp is a real host epoch (the
/// scripts-can't-use-time constraint is for workflow scripts, not the host).
pub fn build_prefix_table_sql(rows: &[PrefixRow]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut sql = String::new();
    sql.push_str(PREFIX_SCHEMA_SQL);
    // Distinct prefix rows (one per (name, expansion) pair seen).
    let mut prefixes: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.prefix.as_str(), r.expansion.as_str()))
        .collect();
    prefixes.sort();
    prefixes.dedup();
    for (name, expansion) in prefixes {
        // INSERT OR IGNORE so an existing operator-set row / description is kept.
        sql.push_str(&format!(
            "INSERT OR IGNORE INTO __ducklink_prefix(name, expansion, created_at) \
             VALUES ('{}', '{}', {now});",
            sql_lit(name),
            sql_lit(expansion),
        ));
    }
    for r in rows {
        sql.push_str(&format!(
            "INSERT OR IGNORE INTO __ducklink_prefix_function\
             (expansion, function_name, extension_name, shape, n_args, registered_at) \
             VALUES ('{}', '{}', '{}', '{}', {}, {now});",
            sql_lit(&r.expansion),
            sql_lit(&r.function_name),
            sql_lit(&r.extension),
            sql_lit(r.shape),
            r.n_args,
        ));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_prefix_accepts_valid_identifiers() {
        assert_eq!(sanitize_prefix("jsonfns").as_deref(), Some("jsonfns"));
        assert_eq!(sanitize_prefix("_foo1").as_deref(), Some("_foo1"));
        assert_eq!(sanitize_prefix("ST_2"), Some("ST_2".to_string()));
        // invalid: starts with digit, contains hyphen / colon
        assert_eq!(sanitize_prefix("2foo"), None);
        assert_eq!(sanitize_prefix("iban-component"), None);
        assert_eq!(sanitize_prefix("foaf:name"), None);
        assert_eq!(sanitize_prefix(""), None);
    }

    #[test]
    fn sanitize_name_coerces_to_identifier() {
        assert_eq!(sanitize_name("iban-component"), "iban_component");
        assert_eq!(sanitize_name("jsonfns"), "jsonfns");
        assert_eq!(sanitize_name("123x"), "_123x");
    }

    #[test]
    fn qualified_name_basic_and_skips() {
        assert_eq!(
            qualified_name("jsonfns", "json_valid").as_deref(),
            Some("jsonfns__json_valid")
        );
        // already-prefixed bare name -> skip to avoid double-prefixing
        assert_eq!(qualified_name("jsonfns", "jsonfns__json_valid"), None);
        // invalid prefix -> skip
        assert_eq!(qualified_name("bad-prefix", "x"), None);
    }

    #[test]
    fn registry_fallback_when_missing() {
        let reg = PrefixRegistry::default();
        let info = reg.resolve("widget");
        assert!(info.is_fallback);
        assert_eq!(info.prefix, "widget");
        assert_eq!(info.expansion, "ducklink-internal://widget");
    }

    #[test]
    fn registry_loaded_entry_wins() {
        let mut entries = HashMap::new();
        entries.insert(
            "jsonfns".to_string(),
            ("jsonfns".to_string(), "com.tegmentum.ducklink.json".to_string()),
        );
        let reg = PrefixRegistry {
            entries,
            warned_fallback: std::cell::RefCell::new(HashSet::new()),
        };
        let info = reg.resolve("jsonfns");
        assert!(!info.is_fallback);
        assert_eq!(info.prefix, "jsonfns");
        assert_eq!(info.expansion, "com.tegmentum.ducklink.json");
    }

    #[test]
    fn collision_tracker_no_collision_same_expansion() {
        let mut t = CollisionTracker::default();
        let info = PrefixInfo {
            prefix: "jsonfns".into(),
            expansion: "com.x.json".into(),
            is_fallback: false,
        };
        // Two registrations from the SAME expansion (e.g. two overloads of the
        // same component) are NOT a cross-component collision.
        let r1 = t.record("jsonfns", "com.x.json", &info, "json_type", Shape::Scalar, 1);
        assert!(!r1.is_collision);
        let r2 = t.record("jsonfns", "com.x.json", &info, "json_type", Shape::Scalar, 2);
        assert!(!r2.is_collision); // different arity anyway
        let r3 = t.record("jsonfns", "com.x.json", &info, "json_type", Shape::Scalar, 1);
        assert!(!r3.is_collision); // same expansion, same key -> not a collision
    }

    #[test]
    fn collision_tracker_detects_cross_expansion() {
        let mut t = CollisionTracker::default();
        let a = PrefixInfo {
            prefix: "luhn".into(),
            expansion: "com.x.luhn".into(),
            is_fallback: false,
        };
        let b = PrefixInfo {
            prefix: "luhngen".into(),
            expansion: "com.x.luhngen".into(),
            is_fallback: false,
        };
        let r1 = t.record("luhn", "com.x.luhn", &a, "luhn_check_digit", Shape::Scalar, 1);
        assert!(!r1.is_collision);
        let r2 = t.record("luhngen", "com.x.luhngen", &b, "luhn_check_digit", Shape::Scalar, 1);
        assert!(r2.is_collision);
        assert_eq!(r2.colliding_extensions, vec!["luhn", "luhngen"]);
        // bare owner is the last loaded (luhngen)
        let warning = format_collision_warning(&r2);
        assert!(warning.contains("luhn_check_digit"));
        assert!(warning.contains("luhngen__luhn_check_digit"));
        assert!(warning.contains("luhn__luhn_check_digit"));
        assert!(warning.contains("resolves to 'luhngen'"));
    }
}
