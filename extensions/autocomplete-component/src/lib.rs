//! SQL auto-completion as a DuckDB table function (a tractable subset of the
//! official `autocomplete` extension's `sql_auto_complete`):
//!
//!   sql_complete(partial VARCHAR) -> table(
//!       suggestion VARCHAR,   -- a completion for the last token of `partial`
//!       kind       VARCHAR)   -- always 'keyword' here
//!
//! It takes the LAST whitespace-delimited token of `partial` and returns every
//! bundled SQL keyword that has that token as a case-insensitive prefix, in
//! sorted order, kind='keyword'. An empty/whitespace-only last token returns the
//! full keyword list. NULL or a missing argument -> zero rows (never a panic).
//!
//! SCOPE / honesty: this is keyword + last-token-prefix completion only. The
//! official extension also completes catalog names (tables/columns) and is
//! context-aware (it knows a table name is expected after FROM, a column after
//! SELECT, etc.). Both of those need DuckDB's parser/catalog: the former wants a
//! live-connection query import (e.g. `SELECT table_name FROM duckdb_tables()`),
//! the latter wants parser state. Neither is reachable from this component's WIT
//! world -- the runtime/catalog imports only register functions/types/casts/
//! macros; there is no live-query / spi import here -- so catalog-name and
//! context-aware completion are intentionally out of scope and remain core-bound.
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

mod core {
    //! Pure-Rust completion logic, free of WIT types, so it can be unit tested
    //! natively.

    /// Bundled SQL keyword list. Multi-word forms (GROUP BY, ORDER BY, ...) are
    /// included as single suggestions so completing "GR" yields "GROUP BY".
    /// Sorted lexicographically so output ordering is stable without a re-sort.
    pub const KEYWORDS: &[&str] = &[
        "ALTER",
        "AND",
        "AS",
        "ASC",
        "BETWEEN",
        "BY",
        "CASE",
        "CAST",
        "CREATE",
        "CROSS JOIN",
        "DELETE",
        "DESC",
        "DISTINCT",
        "DROP",
        "ELSE",
        "END",
        "EXCEPT",
        "EXISTS",
        "FROM",
        "FULL JOIN",
        "GROUP BY",
        "HAVING",
        "IN",
        "INNER JOIN",
        "INSERT",
        "INTERSECT",
        "INTO",
        "IS",
        "JOIN",
        "LEFT JOIN",
        "LIKE",
        "LIMIT",
        "NOT",
        "NULL",
        "OFFSET",
        "ON",
        "OR",
        "ORDER BY",
        "OUTER JOIN",
        "PRAGMA",
        "RIGHT JOIN",
        "SELECT",
        "SET",
        "TABLE",
        "THEN",
        "UNION",
        "UPDATE",
        "USING",
        "VALUES",
        "VIEW",
        "WHEN",
        "WHERE",
        "WITH",
    ];

    /// The last whitespace-delimited token of `partial`. Empty string if
    /// `partial` is empty or ends in whitespace -- a trailing space means the
    /// user has started a NEW token, so an empty prefix (suggest everything) is
    /// the right answer there. `split_whitespace().last()` would instead strip
    /// the trailing space and re-return the prior token, so guard for it.
    pub fn last_token(partial: &str) -> &str {
        if partial.is_empty() || partial.ends_with(char::is_whitespace) {
            return "";
        }
        partial.split_whitespace().last().unwrap_or("")
    }

    /// Keywords whose name starts with `token` (case-insensitive). An empty
    /// token matches everything. Results preserve KEYWORDS' sorted order.
    pub fn complete(partial: &str) -> std::vec::Vec<&'static str> {
        let token = last_token(partial).to_ascii_uppercase();
        KEYWORDS
            .iter()
            .filter(|kw| kw.starts_with(&token))
            .copied()
            .collect()
    }
}

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_sql_complete()?;
        Ok(types::Loadresult {
            name: "autocomplete".into(),
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
        Err(types::Duckerror::Unsupported("autocomplete: no scalar fns".into()))
    }
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("autocomplete: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // single registered table fn; any known handle maps to sql_complete
        let _ = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        // NULL / missing arg -> zero rows (proves wiring, never panics).
        let partial = match args.into_iter().next() {
            Some(types::Duckvalue::Text(s)) => s,
            Some(types::Duckvalue::Null) | None => return Ok(Vec::new().into()),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "sql_complete expects a single VARCHAR argument".into(),
                ))
            }
        };

        let rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = core::complete(&partial)
            .into_iter()
            .map(|kw| {
                vec![
                    types::Duckvalue::Text(kw.into()),
                    types::Duckvalue::Text("keyword".into()),
                ]
            })
            .collect();
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("autocomplete: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("autocomplete: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("autocomplete: no casts".into()))
    }
}

export!(Extension);

fn register_sql_complete() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, T::SqlComplete);

    let args = vec![runtime::Funcarg {
        name: Some("partial".into()),
        logical: types::Logicaltype::Text,
    }];
    let columns = vec![
        types::Columndef {
            name: "suggestion".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "kind".into(),
            logical: types::Logicaltype::Text,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Suggest SQL keyword completions for the last token of a partial \
             query: sql_complete(partial) -> (suggestion, kind='keyword')"
                .into(),
        ),
        tags: vec!["autocomplete".into(), "sql".into()],
    };
    reg.register(
        "sql_complete",
        &args,
        &columns,
        runtime::TableCallback::new(h),
        Some(&opts),
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    SqlComplete,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::core;

    #[test]
    fn sel_completes_select() {
        // 'SEL' -> 'SELECT' (and nothing else with that prefix).
        assert_eq!(core::complete("SEL"), vec!["SELECT"]);
    }

    #[test]
    fn lowercase_prefix_is_case_insensitive() {
        assert_eq!(core::complete("sel"), vec!["SELECT"]);
    }

    #[test]
    fn uses_last_token_only() {
        // last token 'GR' -> 'GROUP BY' (earlier tokens are ignored).
        assert_eq!(core::complete("FROM x WHERE a GR"), vec!["GROUP BY"]);
    }

    #[test]
    fn prefix_can_match_several() {
        // 'IN' prefixes IN, INNER JOIN, INSERT, INTERSECT, INTO (sorted).
        assert_eq!(
            core::complete("IN"),
            vec!["IN", "INNER JOIN", "INSERT", "INTERSECT", "INTO"]
        );
    }

    #[test]
    fn no_match_yields_empty() {
        assert!(core::complete("ZZZ").is_empty());
    }

    #[test]
    fn empty_last_token_returns_all() {
        // trailing space -> empty last token -> the whole keyword list.
        assert_eq!(core::complete("SELECT * FROM t ").len(), core::KEYWORDS.len());
        assert_eq!(core::complete("").len(), core::KEYWORDS.len());
    }

    #[test]
    fn keywords_are_sorted_and_unique() {
        // Guards the "sorted order without a re-sort" invariant complete() relies on.
        let mut sorted = core::KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(core::KEYWORDS.to_vec(), sorted, "KEYWORDS must be sorted");
        sorted.dedup();
        assert_eq!(sorted.len(), core::KEYWORDS.len(), "KEYWORDS must be unique");
    }
}
