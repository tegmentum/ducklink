//! Host-side `delta_scan('dir')` -> `read_parquet([...])` rewrite.
//!
//! The wasm core flattens table-function arguments to literals at plan time, so
//! `delta_log_info((SELECT ... read_text ...))` cannot bind: a subquery-valued
//! table-fn arg fails. To still offer a ONE-query data scan, the host (which has
//! real filesystem access plus the host->guest preopen mapping) intercepts
//! `delta_scan('<guest-dir>')` in `HostState::execute`, reads the table's
//! `_delta_log/*.json`, computes the ACTIVE file set (add minus remove, honoring
//! the log -- NOT a blind *.parquet glob), and rewrites the call to a
//! `read_parquet([...])` over those active files. The core then reads the
//! parquet through its already-working WasmFileSystem preopen access.
//!
//! Scope: this is the CLI/host path. A core-side `delta_scan` TableFunction (or
//! replacement scan) would be needed for the served/web (non-host-mediated)
//! paths; see the report. The same shape extends to iceberg
//! (icebergscan metadata -> avrofns manifests -> read_parquet).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The active file set resolved from a Delta `_delta_log`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeltaActiveFiles {
    /// Active data-file paths, relative to the table directory, in the order
    /// their `add` actions first appear (a remove of a path drops it).
    pub paths: Vec<String>,
}

/// Parse a concatenated `_delta_log` (JSON-lines across all commit files, in
/// version order) into the ACTIVE add-file set: a path becomes active on `add`
/// and inactive on `remove`. Later commits win, so we process lines in order and
/// track an insertion-ordered active set. Blank / non-JSON / non-object lines
/// are skipped (never a panic) -- same robustness contract as the deltascan
/// component's parser.
pub fn active_files(log: &str) -> DeltaActiveFiles {
    // insertion order preserved; membership tracked alongside.
    let mut order: Vec<String> = Vec::new();
    let mut active: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(m)) => m,
            _ => continue,
        };
        if let Some(path) = obj
            .get("add")
            .and_then(|a| a.get("path"))
            .and_then(Value::as_str)
        {
            if active.insert(path.to_string()) {
                order.push(path.to_string());
            }
        }
        if let Some(path) = obj
            .get("remove")
            .and_then(|r| r.get("path"))
            .and_then(Value::as_str)
        {
            active.remove(path);
        }
    }

    DeltaActiveFiles {
        paths: order.into_iter().filter(|p| active.contains(p)).collect(),
    }
}

/// Read + concatenate the table's `_delta_log/*.json` commit files from the HOST
/// filesystem in version order (filename sort). `table_host_dir` is the real
/// host path of the table directory. Missing `_delta_log` -> empty string.
pub fn read_log_from_host(table_host_dir: &Path) -> std::io::Result<String> {
    let log_dir = table_host_dir.join("_delta_log");
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(&log_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect(),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e),
    };
    // Delta encodes version in the zero-padded filename; lexical sort == version
    // order.
    entries.sort();
    let mut log = String::new();
    for p in entries {
        let body = std::fs::read_to_string(&p)?;
        log.push_str(&body);
        if !body.ends_with('\n') {
            log.push('\n');
        }
    }
    Ok(log)
}

/// A single `delta_scan('arg')` occurrence found in a SQL string.
#[derive(Debug, PartialEq, Eq)]
pub struct DeltaScanCall {
    /// Byte range of the whole `delta_scan('arg')` call in the source SQL.
    pub start: usize,
    pub end: usize,
    /// The single-quoted directory argument (already unescaped of `''`).
    pub dir: String,
}

/// Find every top-level `delta_scan('...')` call with a single string-literal
/// argument. Case-insensitive on the function name; tolerant of whitespace
/// between the name, the paren, and the literal. Only the simple
/// single-string-arg form is rewritten; anything else (e.g. an already-subquery
/// arg, multiple args) is left untouched for the core to reject as before.
pub fn find_delta_scan_calls(sql: &str) -> Vec<DeltaScanCall> {
    let bytes = sql.as_bytes();
    let lower = sql.to_ascii_lowercase();
    let needle = "delta_scan";
    let mut calls = Vec::new();
    let mut search_from = 0;

    while let Some(rel) = lower[search_from..].find(needle) {
        let name_start = search_from + rel;
        let after_name = name_start + needle.len();
        search_from = after_name;

        // Must be a standalone identifier: the char before must not be an
        // identifier char (so we don't match `my_delta_scan`).
        if name_start > 0 {
            let prev = bytes[name_start - 1];
            if prev == b'_' || prev.is_ascii_alphanumeric() {
                continue;
            }
        }

        // Skip whitespace, then require '('.
        let mut i = after_name;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'(' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // Require a single-quote string literal.
        if i >= bytes.len() || bytes[i] != b'\'' {
            continue;
        }
        i += 1;
        let mut dir = String::new();
        let mut closed = false;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\'' {
                // SQL escapes a quote by doubling it.
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    dir.push('\'');
                    i += 2;
                    continue;
                }
                i += 1;
                closed = true;
                break;
            }
            dir.push(c as char);
            i += 1;
        }
        if !closed {
            continue;
        }
        // Skip whitespace, then require ')'.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b')' {
            continue;
        }
        i += 1; // consume ')'
        calls.push(DeltaScanCall {
            start: name_start,
            end: i,
            dir,
        });
        search_from = i;
    }
    calls
}

/// Resolve a `delta_scan` guest-dir argument to its host path using the
/// host->guest preopen mapping. `dir` is the guest path the user typed (e.g.
/// `data` or `data/sub`). Returns (host_dir, guest_dir) where guest_dir is the
/// normalized guest path to use when building read_parquet file paths.
pub fn resolve_guest_dir<'a>(
    dir: &str,
    preopens: &'a [(PathBuf, String)],
) -> Option<(PathBuf, String)> {
    let dir_norm = dir.trim_end_matches('/');
    // Longest-guest-prefix match wins so a deeper mount beats "." .
    let mut best: Option<(&'a PathBuf, &'a str, &str)> = None;
    for (host, guest) in preopens {
        let g = guest.trim_end_matches('/');
        let g = if g == "." { "" } else { g };
        let rel: Option<&str> = if g.is_empty() {
            Some(dir_norm)
        } else if dir_norm == g {
            Some("")
        } else if let Some(stripped) = dir_norm.strip_prefix(g) {
            stripped.strip_prefix('/')
        } else {
            None
        };
        if let Some(rel) = rel {
            let better = match best {
                None => true,
                Some((_, bg, _)) => g.len() > bg.len(),
            };
            if better {
                best = Some((host, g, rel));
            }
        }
    }
    let (host, _g, rel) = best?;
    let host_dir = if rel.is_empty() {
        host.clone()
    } else {
        host.join(rel)
    };
    Some((host_dir, dir_norm.to_string()))
}

/// Build the `read_parquet([...])` replacement SQL for an active file set under
/// a guest directory. Returns a parenthesized subquery so it can drop into any
/// `FROM` position. Empty file set -> a typed-but-empty scan is impossible
/// without a schema, so we emit a zero-row VALUES-free query the core accepts:
/// `read_parquet([])` errors in duckdb, so instead select nothing.
pub fn build_read_parquet(guest_dir: &str, files: &[String]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let guest = guest_dir.trim_end_matches('/');
    let list = files
        .iter()
        .map(|f| {
            let full = if guest.is_empty() {
                f.clone()
            } else {
                format!("{guest}/{f}")
            };
            format!("'{}'", full.replace('\'', "''"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("(SELECT * FROM read_parquet([{list}]))"))
}

/// Rewrite all simple `delta_scan('dir')` calls in `sql` to `read_parquet`
/// subqueries, reading each table's `_delta_log` from the host filesystem.
/// Returns the rewritten SQL (unchanged if there are no rewritable calls).
/// On a per-call resolution failure (unknown mount, no active files), the call
/// is left as-is so the core produces its normal error.
pub fn rewrite_delta_scan(sql: &str, preopens: &[(PathBuf, String)]) -> String {
    let calls = find_delta_scan_calls(sql);
    if calls.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    for call in calls {
        let replacement = (|| {
            let (host_dir, guest_dir) = resolve_guest_dir(&call.dir, preopens)?;
            let log = read_log_from_host(&host_dir).ok()?;
            let active = active_files(&log);
            build_read_parquet(&guest_dir, &active.paths)
        })();
        out.push_str(&sql[cursor..call.start]);
        match replacement {
            Some(repl) => out.push_str(&repl),
            None => out.push_str(&sql[call.start..call.end]),
        }
        cursor = call.end;
    }
    out.push_str(&sql[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = concat!(
        r#"{"commitInfo":{"timestamp":1}}"#,
        "\n",
        r#"{"protocol":{"minReaderVersion":1,"minWriterVersion":2}}"#,
        "\n",
        r#"{"metaData":{"schemaString":"{}"}}"#,
        "\n",
        r#"{"add":{"path":"part-0.parquet","size":10}}"#,
        "\n",
    );

    #[test]
    fn active_is_add_minus_remove() {
        let log = concat!(
            r#"{"add":{"path":"a.parquet"}}"#,
            "\n",
            r#"{"add":{"path":"b.parquet"}}"#,
            "\n",
            r#"{"remove":{"path":"a.parquet"}}"#,
            "\n",
        );
        let af = active_files(log);
        assert_eq!(af.paths, vec!["b.parquet".to_string()]);
    }

    #[test]
    fn active_single_add() {
        let af = active_files(LOG);
        assert_eq!(af.paths, vec!["part-0.parquet".to_string()]);
    }

    #[test]
    fn malformed_lines_skipped() {
        let log = "garbage\n\n[1,2]\n{\"add\":{\"path\":\"p.parquet\"}}";
        assert_eq!(active_files(log).paths, vec!["p.parquet".to_string()]);
        assert!(active_files("").paths.is_empty());
    }

    #[test]
    fn find_simple_call() {
        let calls = find_delta_scan_calls("SELECT * FROM delta_scan('data') ORDER BY 1");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].dir, "data");
        let c = &calls[0];
        assert_eq!(&"SELECT * FROM delta_scan('data') ORDER BY 1"[c.start..c.end], "delta_scan('data')");
    }

    #[test]
    fn case_insensitive_and_spaced() {
        let calls = find_delta_scan_calls("from DELTA_SCAN ( 'd/x' )");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].dir, "d/x");
    }

    #[test]
    fn does_not_match_substring_identifier() {
        assert!(find_delta_scan_calls("SELECT my_delta_scan('x')").is_empty());
    }

    #[test]
    fn ignores_subquery_arg() {
        assert!(find_delta_scan_calls("delta_scan((SELECT x))").is_empty());
    }

    #[test]
    fn resolve_named_mount() {
        let pre = vec![(PathBuf::from("/host/tbl"), "data".to_string())];
        let (h, g) = resolve_guest_dir("data", &pre).unwrap();
        assert_eq!(h, PathBuf::from("/host/tbl"));
        assert_eq!(g, "data");
    }

    #[test]
    fn resolve_subdir_under_mount() {
        let pre = vec![(PathBuf::from("/host/root"), "data".to_string())];
        let (h, g) = resolve_guest_dir("data/sub", &pre).unwrap();
        assert_eq!(h, PathBuf::from("/host/root/sub"));
        assert_eq!(g, "data/sub");
    }

    #[test]
    fn build_read_parquet_list() {
        let sql = build_read_parquet("data", &["a.parquet".to_string(), "b.parquet".to_string()]).unwrap();
        assert_eq!(sql, "(SELECT * FROM read_parquet(['data/a.parquet', 'data/b.parquet']))");
    }

    #[test]
    fn rewrite_end_to_end_with_tmpdir() {
        // Build a fake delta table on disk.
        let dir = std::env::temp_dir().join(format!("delta_rw_test_{}", std::process::id()));
        let log_dir = dir.join("_delta_log");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("00000000000000000000.json"),
            "{\"add\":{\"path\":\"part-0.parquet\"}}\n",
        )
        .unwrap();
        let pre = vec![(dir.clone(), "data".to_string())];
        let out = rewrite_delta_scan("SELECT * FROM delta_scan('data') ORDER BY 1", &pre);
        assert_eq!(
            out,
            "SELECT * FROM (SELECT * FROM read_parquet(['data/part-0.parquet'])) ORDER BY 1"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_mount_left_untouched() {
        let pre: Vec<(PathBuf, String)> = vec![];
        let sql = "SELECT * FROM delta_scan('nope')";
        assert_eq!(rewrite_delta_scan(sql, &pre), sql);
    }
}
