//! Phase 2 (@5): SQL text intercept for ATTACH-driven foreign catalogs and
//! for writes against them.
//!
//! The wasm DuckDB core no longer imports the eight `*-host` interfaces (see
//! ADR Decision 3). Instead, the HOST intercepts a small, well-defined SQL
//! surface *before* the SQL reaches the core:
//!
//!  * `ATTACH '<dsn>' AS <alias> (TYPE <type> [, k=v, ...])` — routes to the
//!    storage-capable extension registered under `<type>`. The extension's
//!    `storage-dispatch.storage-attach` opens the foreign catalog and returns
//!    an opaque handle; `storage-list-tables` + `storage-table-columns`
//!    enumerate the schema; scans and writes are routed subsequently.
//!  * `INSERT INTO <alias>.<table> ...` / `UPDATE <alias>.<table> SET ... [WHERE ...]`
//!    / `DELETE FROM <alias>.<table> [WHERE ...]` — where `<alias>` names a
//!    previously-attached foreign catalog. Routed to
//!    `storage-write-dispatch.{insert,update,delete}` on the owning extension
//!    (see ADR Amendment A1 + A5 for the rowid pre-scan design).
//!
//! Explicitly out of scope for v1 (Amendment A2 Risk 8): `RETURNING`,
//! `INSERT ... ON CONFLICT` / `ON DUPLICATE KEY`, CTE-driven writes
//! (`WITH ... INSERT INTO <alias>.<table> ...`), prepared/parameterized DDL
//! (`?` in the write clause), and multi-statement scripts mixing intercepted
//! and non-intercepted writes. Each rejected shape returns a
//! `Duckerror::Unsupported` with a specific reason so callers can distinguish
//! "not a foreign-catalog write" (fall through to the core) from "foreign-
//! catalog write but this pattern is a v1 non-goal".
//!
//! This module is intentionally deps-free (no `regex` / `pest`) so it can be
//! fuzzed cheaply. All parsers are pure functions.

use std::collections::HashMap;

/// A parsed ATTACH statement targeting a storage-capable extension. Only the
/// `TYPE <name>` form is recognized; anything else falls through to the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSpec {
    pub dsn: String,
    pub alias: String,
    pub type_name: String,
    pub options: Vec<(String, String)>,
    pub if_not_exists: bool,
    pub read_only: bool,
}

/// A parsed write against an attached foreign catalog. See A1 + A5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRoute {
    Insert {
        alias: String,
        table: String,
        /// Explicit column list from the INSERT clause. Empty = positional
        /// (rely on the table's natural column order at ATTACH time).
        columns: Vec<String>,
        /// One row per `VALUES (...)` tuple. Every cell is the raw SQL text
        /// of the value literal (unquoted for strings — see `parse_value_literal`).
        rows: Vec<Vec<ValueLiteral>>,
    },
    Update {
        alias: String,
        table: String,
        /// `SET col = <expr>` assignments. `expr` is the raw SQL text of the
        /// right-hand side (rendered back out for the pre-scan / update path).
        assignments: Vec<(String, String)>,
        /// `WHERE <predicate>` if present; raw SQL text.
        where_clause: Option<String>,
    },
    Delete {
        alias: String,
        table: String,
        where_clause: Option<String>,
    },
    /// Recognized as an attached-catalog write, but rejected because the
    /// pattern is a v1 non-goal (see the module docs).
    Unsupported(String),
}

/// A cell literal parsed from `VALUES (...)`. Preserving the type-carrying
/// distinction between NULL / integer / float / string / blob lets the write
/// dispatch route into the right `extension_types::Duckvalue` arm without
/// re-parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueLiteral {
    Null,
    Integer(i64),
    Float(f64),
    String(String),
    Blob(Vec<u8>),
    /// A raw SQL fragment we couldn't further classify (an expression like
    /// `now()` / `col + 1`). The write intercept treats these as unsupported;
    /// the field is retained so the error message can quote the fragment.
    Raw(String),
}

impl Eq for ValueLiteral {}

/// Case-insensitive whitespace-aware helper: returns `true` if `input` (with
/// leading whitespace stripped) starts with `kw` followed by a word boundary.
pub(crate) fn starts_with_kw_ci(input: &str, kw: &str) -> bool {
    let s = input.trim_start();
    if s.len() < kw.len() {
        return false;
    }
    if !s.as_bytes()[..kw.len()].eq_ignore_ascii_case(kw.as_bytes()) {
        return false;
    }
    match s.as_bytes().get(kw.len()) {
        None => true,
        Some(b) => !b.is_ascii_alphanumeric() && *b != b'_',
    }
}

/// Split a SQL identifier of the form `<alias>.<table>` (unquoted) into its
/// two parts. Returns `None` on anything more exotic (schema-qualified,
/// quoted, function calls, etc).
pub(crate) fn split_alias_table(ident: &str) -> Option<(String, String)> {
    let ident = ident.trim();
    let (alias, rest) = ident.split_once('.')?;
    let alias = alias.trim();
    let rest = rest.trim();
    if alias.is_empty() || rest.is_empty() {
        return None;
    }
    // Reject further dotting — no support for main / catalog qualifiers here.
    if rest.contains('.') {
        return None;
    }
    if !is_bare_ident(alias) || !is_bare_ident(rest) {
        return None;
    }
    Some((alias.to_string(), rest.to_string()))
}

pub(crate) fn is_bare_ident(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Skip a SQL comment starting at `bytes[i]` (either `--...\n` or `/* ... */`).
/// Returns the byte index just past the end of the comment, or `i` if there
/// is no comment.
fn skip_comment(bytes: &[u8], i: usize) -> usize {
    if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
        let mut j = i + 2;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        return j;
    }
    if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
        let mut j = i + 2;
        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
            j += 1;
        }
        return (j + 2).min(bytes.len());
    }
    i
}

/// Strip a trailing `;` and any trailing whitespace / SQL comments.
pub(crate) fn strip_trailing_semicolon(s: &str) -> &str {
    let s = s.trim();
    s.strip_suffix(';').unwrap_or(s).trim_end()
}

/// True if `sql` contains more than one non-empty statement separated by `;`.
pub(crate) fn is_multi_statement(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut seen_first_stmt_end = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        let after_comment = skip_comment(bytes, i);
        if after_comment != i {
            i = after_comment;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b';' => {
                if seen_first_stmt_end {
                    // Two `;` = at least two statements delimited.
                    return true;
                }
                seen_first_stmt_end = true;
            }
            _ => {
                if seen_first_stmt_end && !c.is_ascii_whitespace() {
                    // Non-whitespace after the first `;` = second statement.
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

// ---------------------------------------------------------------------------
// ATTACH parser
// ---------------------------------------------------------------------------

/// Recognize `ATTACH '<dsn>' AS <alias> [(TYPE <type>[, k=v ...])]` (also with
/// `IF NOT EXISTS`, `READ_ONLY`). Case-insensitive whitespace-flexible.
/// Returns `None` for any shape we don't recognize — the caller then falls
/// through to the core.
///
/// Only forms carrying `TYPE <name>` are considered intercept candidates;
/// bare `ATTACH ':memory:' AS foo` stays a plain core-driven ATTACH.
pub fn parse_attach(sql: &str) -> Option<AttachSpec> {
    let sql = strip_trailing_semicolon(sql).trim();
    if !starts_with_kw_ci(sql, "ATTACH") {
        return None;
    }
    let rest = &sql[6..].trim_start();
    // Optional `IF NOT EXISTS`.
    let (if_not_exists, rest) = if starts_with_kw_ci(rest, "IF") {
        let after_if = rest[2..].trim_start();
        if starts_with_kw_ci(after_if, "NOT") {
            let after_not = after_if[3..].trim_start();
            if starts_with_kw_ci(after_not, "EXISTS") {
                (true, after_not[6..].trim_start().to_string())
            } else {
                return None;
            }
        } else {
            return None;
        }
    } else {
        (false, rest.to_string())
    };
    // DSN — must be a single-quoted string.
    let (dsn, rest_after_dsn) = parse_single_quoted_string(&rest)?;
    let rest = rest_after_dsn.trim_start();
    // `AS <alias>`.
    if !starts_with_kw_ci(rest, "AS") {
        return None;
    }
    let after_as = rest[2..].trim_start();
    let (alias, rest_after_alias) = parse_bare_ident(after_as)?;
    let rest = rest_after_alias.trim_start();
    // Optional `(TYPE <name>[, k=v ...])`.
    if !rest.starts_with('(') {
        // No option list = plain ATTACH; not an intercept candidate.
        return None;
    }
    let close = rest.rfind(')')?;
    let inside = &rest[1..close];
    let (type_name, options) = parse_attach_options(inside)?;
    // Detect READ_ONLY as either an option or a bare keyword inside `()`.
    let read_only = options
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("read_only"));
    Some(AttachSpec {
        dsn,
        alias,
        type_name,
        options,
        if_not_exists,
        read_only,
    })
}

/// Parse the `(TYPE <name>[, k=v ...])` payload. Returns `None` when TYPE
/// isn't present; extra options are captured as `(k, v)` pairs (both trimmed).
fn parse_attach_options(inside: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut type_name: Option<String> = None;
    let mut options: Vec<(String, String)> = Vec::new();
    for part in split_top_level_commas(inside) {
        let part = part.trim();
        if starts_with_kw_ci(part, "TYPE") {
            let val = part[4..].trim();
            // Support both `TYPE foo` and `TYPE = foo` and `TYPE 'foo'`.
            let val = val.strip_prefix('=').unwrap_or(val).trim();
            let val = strip_quotes(val).unwrap_or_else(|| val.to_string());
            type_name = Some(val);
            continue;
        }
        if starts_with_kw_ci(part, "READ_ONLY") {
            options.push(("read_only".to_string(), "true".to_string()));
            continue;
        }
        if let Some(eq) = part.find('=') {
            let k = part[..eq].trim().to_string();
            let v = part[eq + 1..].trim();
            let v = strip_quotes(v).unwrap_or_else(|| v.to_string());
            options.push((k, v));
        }
    }
    Some((type_name?, options))
}

fn strip_quotes(v: &str) -> Option<String> {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 && bytes.first() == Some(&b'\'') && bytes.last() == Some(&b'\'') {
        return Some(v[1..v.len() - 1].replace("''", "'"));
    }
    None
}

fn parse_single_quoted_string(s: &str) -> Option<(String, String)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            return Some((out, s[i + 1..].to_string()));
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    None
}

fn parse_bare_ident(s: &str) -> Option<(String, String)> {
    let s = s.trim_start();
    let mut it = s.char_indices();
    let (_, first) = it.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut end = first.len_utf8();
    for (i, c) in it {
        if c.is_ascii_alphanumeric() || c == '_' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    Some((s[..end].to_string(), s[end..].to_string()))
}

/// Split on `,` but only at brace-depth 0 (ignore commas inside quotes /
/// parentheses).
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        if in_single {
            buf.push(c);
            if c == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            buf.push(c);
            if c == '"' {
                in_double = false;
            }
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                buf.push(c);
            }
            '"' => {
                in_double = true;
                buf.push(c);
            }
            '(' | '[' => {
                depth += 1;
                buf.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                buf.push(c);
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut buf));
            }
            _ => buf.push(c),
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}

// ---------------------------------------------------------------------------
// Write-path parsers
// ---------------------------------------------------------------------------

/// Recognize INSERT/UPDATE/DELETE against `<alias>.<table>` where `<alias>`
/// is in `attached`. Returns:
///  * `Some(WriteRoute::Insert|Update|Delete)` for a supported shape;
///  * `Some(WriteRoute::Unsupported(reason))` for a shape that IS an
///    attached-catalog write but hits a v1 non-goal (RETURNING, ON CONFLICT,
///    CTE, `?` param). The caller surfaces this as a hard error;
///  * `None` if the statement is not addressed at an attached alias — the
///    caller falls through to the core.
///
/// `attached` is checked case-sensitively (matches how the ATTACH intercept
/// records the alias). SQL keywords are recognized case-insensitively.
pub fn resolve_write_target<S: AsRef<str>>(
    sql: &str,
    attached: &HashMap<String, S>,
) -> Option<WriteRoute> {
    if is_multi_statement(sql) {
        // Peek at the first statement only. If it's an attached-catalog write
        // in a multi-statement script, that's a v1 non-goal.
        if any_write_touches_attached(sql, attached) {
            return Some(WriteRoute::Unsupported(
                "multi-statement scripts mixing intercepted and non-intercepted writes"
                    .to_string(),
            ));
        }
        return None;
    }
    let sql = strip_trailing_semicolon(sql).trim();
    if starts_with_kw_ci(sql, "WITH") {
        if any_write_touches_attached(sql, attached) {
            return Some(WriteRoute::Unsupported(
                "CTE-driven writes against attached foreign catalogs".to_string(),
            ));
        }
        return None;
    }
    if starts_with_kw_ci(sql, "INSERT") {
        return parse_insert(sql, attached);
    }
    if starts_with_kw_ci(sql, "UPDATE") {
        return parse_update(sql, attached);
    }
    if starts_with_kw_ci(sql, "DELETE") {
        return parse_delete(sql, attached);
    }
    None
}

fn any_write_touches_attached<S: AsRef<str>>(
    sql: &str,
    attached: &HashMap<String, S>,
) -> bool {
    // Coarse check: does the SQL text mention `<alias>.<table>` for any
    // registered alias, alongside INSERT/UPDATE/DELETE? Used only to
    // classify UNSUPPORTED cases (CTE + multi-statement) — false positives
    // here just turn an already-questionable statement into a clear error.
    let up = sql.to_ascii_uppercase();
    let mentions_write = ["INSERT", "UPDATE", "DELETE"]
        .iter()
        .any(|kw| up.contains(kw));
    if !mentions_write {
        return false;
    }
    attached.keys().any(|alias| {
        let needle = format!("{}.", alias.to_ascii_uppercase());
        up.contains(&needle)
    })
}

fn parse_insert<S: AsRef<str>>(
    sql: &str,
    attached: &HashMap<String, S>,
) -> Option<WriteRoute> {
    // INSERT [OR ...] INTO <ident> [( <cols> )] VALUES (...), (...)
    // or INSERT INTO <ident> [(cols)] SELECT ... (unsupported for v1)
    let after_insert = sql[6..].trim_start();
    let (or_kind, after_or) = strip_optional_or_clause(after_insert);
    if !starts_with_kw_ci(after_or, "INTO") {
        return None;
    }
    let rest = after_or[4..].trim_start();
    let (ident, rest) = take_dotted_ident(rest);
    let (alias, table) = split_alias_table(ident.as_str())?;
    if !attached.contains_key(&alias) {
        return None;
    }
    // From here on any oddity is a hard error — we've committed to this being
    // an attached-catalog write.
    if let Some(reason) = or_kind {
        return Some(WriteRoute::Unsupported(reason));
    }
    let rest = rest.trim_start();
    // Optional column list.
    let (columns, rest) = if rest.starts_with('(') {
        let close = balanced_close(rest, '(', ')')?;
        let inside = &rest[1..close];
        let cols: Vec<String> = split_top_level_commas(inside)
            .into_iter()
            .map(|c| c.trim().to_string())
            .collect();
        (cols, rest[close + 1..].trim_start().to_string())
    } else {
        (Vec::new(), rest.to_string())
    };
    if !starts_with_kw_ci(&rest, "VALUES") {
        if starts_with_kw_ci(&rest, "SELECT") {
            return Some(WriteRoute::Unsupported(
                "INSERT ... SELECT into attached foreign catalogs".to_string(),
            ));
        }
        if starts_with_kw_ci(&rest, "DEFAULT") {
            return Some(WriteRoute::Unsupported(
                "INSERT ... DEFAULT VALUES into attached foreign catalogs".to_string(),
            ));
        }
        return Some(WriteRoute::Unsupported(format!(
            "INSERT clause after '{alias}.{table}' isn't a supported VALUES form"
        )));
    }
    let after_values = rest[6..].trim_start();
    // RETURNING / ON CONFLICT / ON DUPLICATE — check the FULL rest for a
    // trailing clause after the VALUES tuples.
    if let Some(reason) = detect_trailing_non_goal(after_values) {
        return Some(WriteRoute::Unsupported(reason));
    }
    // Parameter markers.
    if contains_param_marker(after_values) {
        return Some(WriteRoute::Unsupported(
            "prepared/parameterized DDL on attached foreign catalogs".to_string(),
        ));
    }
    // Parse the VALUES list.
    let rows = parse_values_tuples(after_values)?;
    Some(WriteRoute::Insert {
        alias,
        table,
        columns,
        rows,
    })
}

fn parse_update<S: AsRef<str>>(
    sql: &str,
    attached: &HashMap<String, S>,
) -> Option<WriteRoute> {
    let after_update = sql[6..].trim_start();
    let (ident, rest) = take_dotted_ident(after_update);
    let (alias, table) = split_alias_table(ident.as_str())?;
    if !attached.contains_key(&alias) {
        return None;
    }
    let rest = rest.trim_start();
    if !starts_with_kw_ci(rest, "SET") {
        return Some(WriteRoute::Unsupported(
            "UPDATE without SET clause on attached foreign catalog".to_string(),
        ));
    }
    let after_set = rest[3..].trim_start();
    if contains_param_marker(after_set) {
        return Some(WriteRoute::Unsupported(
            "prepared/parameterized DDL on attached foreign catalogs".to_string(),
        ));
    }
    if let Some(reason) = detect_trailing_non_goal(after_set) {
        return Some(WriteRoute::Unsupported(reason));
    }
    // Split off WHERE clause (case-insensitive, respect quotes/parens).
    let (set_body, where_clause) = split_before_kw_ci(after_set, "WHERE");
    let assignments: Vec<(String, String)> = split_top_level_commas(set_body.trim())
        .into_iter()
        .filter_map(|piece| {
            let piece = piece.trim();
            let eq = find_assignment_equals(piece)?;
            let col = piece[..eq].trim();
            if !is_bare_ident(col) {
                return None;
            }
            let expr = piece[eq + 1..].trim();
            Some((col.to_string(), expr.to_string()))
        })
        .collect();
    if assignments.is_empty() {
        return Some(WriteRoute::Unsupported(
            "UPDATE with no parseable SET assignments on attached foreign catalog".to_string(),
        ));
    }
    Some(WriteRoute::Update {
        alias,
        table,
        assignments,
        where_clause,
    })
}

fn parse_delete<S: AsRef<str>>(
    sql: &str,
    attached: &HashMap<String, S>,
) -> Option<WriteRoute> {
    let after_delete = sql[6..].trim_start();
    if !starts_with_kw_ci(after_delete, "FROM") {
        return None;
    }
    let rest = after_delete[4..].trim_start();
    let (ident, rest) = take_dotted_ident(rest);
    let (alias, table) = split_alias_table(ident.as_str())?;
    if !attached.contains_key(&alias) {
        return None;
    }
    let rest = rest.trim_start();
    if contains_param_marker(rest) {
        return Some(WriteRoute::Unsupported(
            "prepared/parameterized DDL on attached foreign catalogs".to_string(),
        ));
    }
    if let Some(reason) = detect_trailing_non_goal(rest) {
        return Some(WriteRoute::Unsupported(reason));
    }
    let where_clause = if starts_with_kw_ci(rest, "WHERE") {
        Some(rest[5..].trim().to_string())
    } else if rest.is_empty() {
        None
    } else {
        // Something after the table name that isn't WHERE — probably USING /
        // RETURNING (handled above) / etc. Reject rather than silently drop.
        return Some(WriteRoute::Unsupported(format!(
            "DELETE clause '{}' isn't a supported plain WHERE form", rest
        )));
    };
    Some(WriteRoute::Delete {
        alias,
        table,
        where_clause,
    })
}

/// Detect a trailing clause we explicitly refuse: RETURNING, ON CONFLICT,
/// ON DUPLICATE KEY. Returns the specific reason for the error.
fn detect_trailing_non_goal(sql: &str) -> Option<String> {
    let up = sql.to_ascii_uppercase();
    if up.contains("RETURNING") {
        return Some("RETURNING clause on attached foreign catalogs".to_string());
    }
    if up.contains("ON CONFLICT") {
        return Some("INSERT ... ON CONFLICT on attached foreign catalogs".to_string());
    }
    if up.contains("ON DUPLICATE KEY") {
        return Some("INSERT ... ON DUPLICATE KEY on attached foreign catalogs".to_string());
    }
    None
}

/// A very restricted `?` / `$N` parameter marker probe. Skips over
/// quoted string literals so `'?'` doesn't false-trigger.
fn contains_param_marker(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'?' => return true,
            b'$' => {
                if bytes.get(i + 1).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Consume an INSERT `OR X` variant. Returns `(reason-if-unsupported,
/// remaining-sql-after-OR-clause)`.
fn strip_optional_or_clause(sql: &str) -> (Option<String>, &str) {
    if !starts_with_kw_ci(sql, "OR") {
        return (None, sql);
    }
    let after_or = sql[2..].trim_start();
    for kw in ["REPLACE", "IGNORE", "FAIL", "ABORT", "ROLLBACK"] {
        if starts_with_kw_ci(after_or, kw) {
            let rest = after_or[kw.len()..].trim_start();
            return (
                Some(format!("INSERT OR {kw} on attached foreign catalogs")),
                rest,
            );
        }
    }
    (None, sql)
}

/// Take a dotted identifier like `mydb.foo` from the start of `sql`. Returns
/// `(ident, remaining-sql)`. Quoted identifiers stay together.
fn take_dotted_ident(sql: &str) -> (String, String) {
    let sql = sql.trim_start();
    let bytes = sql.as_bytes();
    let mut end = 0;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'"' {
            end += 1;
        } else {
            break;
        }
    }
    (sql[..end].to_string(), sql[end..].to_string())
}

/// Split `s` before the first case-insensitive whole-word occurrence of `kw`
/// at brace-depth 0, ignoring occurrences inside quoted strings.
fn split_before_kw_ci(s: &str, kw: &str) -> (String, Option<String>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if depth == 0 => {
                let is_word_start =
                    i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                let candidate_len = kw.len();
                if is_word_start
                    && i + candidate_len <= bytes.len()
                    && bytes[i..i + candidate_len].eq_ignore_ascii_case(kw.as_bytes())
                {
                    let is_word_end = bytes
                        .get(i + candidate_len)
                        .map(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                        .unwrap_or(true);
                    if is_word_end {
                        let before = s[..i].to_string();
                        let after = s[i + candidate_len..].trim().to_string();
                        return (before, Some(after));
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    (s.to_string(), None)
}

/// Find the `=` byte that splits a SET assignment (skipping `=` occurrences
/// inside strings / parens; NOT skipping `==` — plain SQL doesn't use it).
fn find_assignment_equals(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    for (i, &c) in bytes.iter().enumerate() {
        if in_single {
            if c == b'\'' && bytes.get(i + 1) != Some(&b'\'') {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'=' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

fn balanced_close(sql: &str, open: char, close: char) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes.first() != Some(&(open as u8)) {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    for (i, &c) in bytes.iter().enumerate() {
        if in_single {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    continue;
                }
                in_single = false;
            }
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b if b == open as u8 => depth += 1,
            b if b == close as u8 => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_values_tuples(sql: &str) -> Option<Vec<Vec<ValueLiteral>>> {
    let mut out: Vec<Vec<ValueLiteral>> = Vec::new();
    let mut i = 0;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b',' {
            i += 1;
            continue;
        }
        if bytes[i] == b';' {
            break;
        }
        if bytes[i] != b'(' {
            // Trailing junk after the last tuple — bail out but keep what we got.
            return if out.is_empty() { None } else { Some(out) };
        }
        let close = balanced_close(&sql[i..], '(', ')').map(|c| i + c)?;
        let inside = &sql[i + 1..close];
        let row: Vec<ValueLiteral> = split_top_level_commas(inside)
            .into_iter()
            .map(|c| parse_value_literal(c.trim()))
            .collect();
        out.push(row);
        i = close + 1;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Classify a single VALUES cell into a typed `ValueLiteral`. Unknown or
/// expression-shaped cells (function calls, arithmetic) fall through to
/// `Raw`, which the intercept treats as unsupported.
pub fn parse_value_literal(s: &str) -> ValueLiteral {
    let s = s.trim();
    if s.eq_ignore_ascii_case("NULL") {
        return ValueLiteral::Null;
    }
    if let Some(rest) = s.strip_prefix('\'') {
        if let Some(stripped) = rest.strip_suffix('\'') {
            return ValueLiteral::String(stripped.replace("''", "'"));
        }
    }
    // `X'..'` blob literal.
    if s.starts_with("X'") || s.starts_with("x'") {
        if let Some(inner) = s[2..].strip_suffix('\'') {
            if let Ok(bytes) = decode_hex(inner) {
                return ValueLiteral::Blob(bytes);
            }
        }
    }
    if let Ok(i) = s.parse::<i64>() {
        return ValueLiteral::Integer(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return ValueLiteral::Float(f);
    }
    ValueLiteral::Raw(s.to_string())
}

fn decode_hex(s: &str) -> Result<Vec<u8>, ()> {
    if s.len() % 2 != 0 {
        return Err(());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(10 + b - b'a'),
        b'A'..=b'F' => Ok(10 + b - b'A'),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attached_map(aliases: &[&str]) -> HashMap<String, String> {
        aliases
            .iter()
            .map(|a| (a.to_string(), String::new()))
            .collect()
    }

    #[test]
    fn parse_attach_basic() {
        let s = parse_attach("ATTACH 'test.db' AS mydb (TYPE sqlitewasm)").unwrap();
        assert_eq!(s.dsn, "test.db");
        assert_eq!(s.alias, "mydb");
        assert_eq!(s.type_name, "sqlitewasm");
        assert!(!s.if_not_exists);
        assert!(!s.read_only);
        assert!(s.options.is_empty());
    }

    #[test]
    fn parse_attach_case_insensitive_and_options() {
        let s = parse_attach(
            "  attach 'file.db'  as MyDB ( type = 'sqlitewasm' , read_only , foo=bar );",
        )
        .unwrap();
        assert_eq!(s.dsn, "file.db");
        assert_eq!(s.alias, "MyDB");
        assert_eq!(s.type_name, "sqlitewasm");
        assert!(s.read_only);
        assert!(s.options.iter().any(|(k, v)| k == "foo" && v == "bar"));
    }

    #[test]
    fn parse_attach_if_not_exists() {
        let s = parse_attach("ATTACH IF NOT EXISTS 'a' AS x (TYPE q)").unwrap();
        assert!(s.if_not_exists);
    }

    #[test]
    fn parse_attach_declines_plain_memory_attach() {
        // No option list => not an intercept candidate.
        assert!(parse_attach("ATTACH ':memory:' AS foo").is_none());
    }

    #[test]
    fn parse_attach_declines_wrong_shape() {
        assert!(parse_attach("SELECT 1").is_none());
        assert!(parse_attach("ATTACH 'x'").is_none());
        assert!(parse_attach("ATTACH foo").is_none());
    }

    #[test]
    fn resolve_insert_values_ok() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo (id, name) VALUES (1, 'x')",
            &att,
        )
        .unwrap();
        match r {
            WriteRoute::Insert { alias, table, columns, rows } => {
                assert_eq!(alias, "mydb");
                assert_eq!(table, "foo");
                assert_eq!(columns, vec!["id", "name"]);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], ValueLiteral::Integer(1));
                assert_eq!(rows[0][1], ValueLiteral::String("x".into()));
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn resolve_insert_multi_row() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo VALUES (1, 'a'), (2, 'b'), (NULL, NULL)",
            &att,
        )
        .unwrap();
        match r {
            WriteRoute::Insert { rows, columns, .. } => {
                assert!(columns.is_empty());
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[2][0], ValueLiteral::Null);
                assert_eq!(rows[2][1], ValueLiteral::Null);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn resolve_update_where_ok() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "UPDATE mydb.foo SET name = 'y' WHERE id = 1",
            &att,
        )
        .unwrap();
        match r {
            WriteRoute::Update { alias, table, assignments, where_clause } => {
                assert_eq!(alias, "mydb");
                assert_eq!(table, "foo");
                assert_eq!(assignments, vec![("name".into(), "'y'".into())]);
                assert_eq!(where_clause.as_deref(), Some("id = 1"));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn resolve_delete_where_ok() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target("DELETE FROM mydb.foo WHERE id = 1", &att).unwrap();
        match r {
            WriteRoute::Delete { alias, table, where_clause } => {
                assert_eq!(alias, "mydb");
                assert_eq!(table, "foo");
                assert_eq!(where_clause.as_deref(), Some("id = 1"));
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn resolve_delete_no_where() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target("DELETE FROM mydb.foo", &att).unwrap();
        match r {
            WriteRoute::Delete { where_clause, .. } => assert!(where_clause.is_none()),
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn resolve_declines_non_attached_alias() {
        let att = attached_map(&["mydb"]);
        assert!(resolve_write_target("INSERT INTO other.foo VALUES (1)", &att).is_none());
        assert!(resolve_write_target("UPDATE other.foo SET x = 1", &att).is_none());
        assert!(resolve_write_target("DELETE FROM other.foo", &att).is_none());
    }

    #[test]
    fn resolve_declines_non_write_sql() {
        let att = attached_map(&["mydb"]);
        assert!(resolve_write_target("SELECT * FROM mydb.foo", &att).is_none());
    }

    // ---- Rejection tests (v1 non-goals per ADR Amendment A2 Risk 8) ----

    #[test]
    fn reject_returning() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo (id) VALUES (1) RETURNING id",
            &att,
        )
        .unwrap();
        assert!(matches!(r, WriteRoute::Unsupported(reason) if reason.contains("RETURNING")));
    }

    #[test]
    fn reject_on_conflict() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo (id) VALUES (1) ON CONFLICT DO NOTHING",
            &att,
        )
        .unwrap();
        assert!(matches!(r, WriteRoute::Unsupported(reason) if reason.contains("ON CONFLICT")));
    }

    #[test]
    fn reject_on_duplicate_key() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo (id) VALUES (1) ON DUPLICATE KEY UPDATE id = 2",
            &att,
        )
        .unwrap();
        assert!(matches!(r, WriteRoute::Unsupported(reason) if reason.contains("ON DUPLICATE")));
    }

    #[test]
    fn reject_cte_insert() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "WITH x AS (SELECT 1) INSERT INTO mydb.foo SELECT * FROM x",
            &att,
        )
        .unwrap();
        assert!(matches!(r, WriteRoute::Unsupported(reason) if reason.contains("CTE")));
    }

    #[test]
    fn reject_insert_select() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo SELECT * FROM other_source",
            &att,
        )
        .unwrap();
        assert!(matches!(r, WriteRoute::Unsupported(reason) if reason.contains("SELECT")));
    }

    #[test]
    fn reject_insert_or_replace() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT OR REPLACE INTO mydb.foo VALUES (1)",
            &att,
        )
        .unwrap();
        assert!(matches!(r, WriteRoute::Unsupported(reason) if reason.contains("OR REPLACE")));
    }

    #[test]
    fn reject_parameterized() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "INSERT INTO mydb.foo VALUES (?)",
            &att,
        )
        .unwrap();
        assert!(matches!(
            r,
            WriteRoute::Unsupported(reason) if reason.contains("prepared/parameterized")
        ));
    }

    #[test]
    fn reject_multi_statement_attached_write() {
        let att = attached_map(&["mydb"]);
        let r = resolve_write_target(
            "SELECT 1; INSERT INTO mydb.foo VALUES (1)",
            &att,
        )
        .unwrap();
        assert!(matches!(
            r,
            WriteRoute::Unsupported(reason) if reason.contains("multi-statement")
        ));
    }

    #[test]
    fn multi_statement_untouching_alias_falls_through() {
        let att = attached_map(&["mydb"]);
        assert!(
            resolve_write_target("SELECT 1; SELECT 2", &att).is_none(),
            "read-only multi-statement scripts without attached-alias writes should fall through"
        );
    }

    #[test]
    fn parse_value_literal_arms() {
        assert_eq!(parse_value_literal("NULL"), ValueLiteral::Null);
        assert_eq!(parse_value_literal("42"), ValueLiteral::Integer(42));
        assert_eq!(parse_value_literal("-3"), ValueLiteral::Integer(-3));
        assert_eq!(parse_value_literal("3.14"), ValueLiteral::Float(3.14));
        assert_eq!(parse_value_literal("'hi'"), ValueLiteral::String("hi".into()));
        assert_eq!(
            parse_value_literal("'it''s'"),
            ValueLiteral::String("it's".into()),
        );
        assert_eq!(
            parse_value_literal("X'DEADBEEF'"),
            ValueLiteral::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        );
        assert!(matches!(parse_value_literal("now()"), ValueLiteral::Raw(_)));
    }

    #[test]
    fn multi_statement_probe() {
        assert!(is_multi_statement("SELECT 1; SELECT 2"));
        assert!(!is_multi_statement("SELECT 1;"));
        assert!(!is_multi_statement("SELECT 1"));
        // A `;` inside a string doesn't count.
        assert!(!is_multi_statement("SELECT 'a;b'"));
    }
}
