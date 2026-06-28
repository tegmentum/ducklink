//! ggsql / VISUALIZE parser-extension PoC (2.3.0 / v3).
//!
//! Demonstrates the constrained-but-complete parser surface: the host wires a
//! DuckDB `ParserExtension` that forwards a statement the built-in parser rejected
//! to `parser-dispatch.call-parse`; this component recognizes a `VISUALIZE <q>`
//! statement and rewrites it to ordinary DuckDB SQL the core runs in its place.
//! No bound parse tree crosses the WIT boundary -- text in, SQL text out (the
//! by-value-safe form, see wit/parser-dispatch.wit).
//!
//! `VISUALIZE <select-stmt>`  ->  a SQL script that wraps the inner select and
//! returns an ASCII bar-chart-friendly "(label, n, bar)" rollup -- a tiny stand-in
//! for a real visualization, enough to prove the rewrite path is exercised.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-parser" });

use duckdb::extension::{parser, types};
use exports::duckdb::extension::{callback_dispatch, guest, parser_dispatch};

/// Opaque handle the host threads back into call-parse.
const PARSER_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        parser::register_parser_extension("ggsql", PARSER_HANDLE)?;
        Ok(types::Loadresult {
            name: "ggsql".into(),
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

impl parser_dispatch::Guest for Extension {
    fn call_parse(
        handle: u32,
        query: String,
    ) -> Result<parser_dispatch::ParseOutcome, types::Duckerror> {
        if handle != PARSER_HANDLE {
            return Ok(parser_dispatch::ParseOutcome::Declined);
        }
        let q = query.to_string();
        let trimmed = q.trim().trim_end_matches(';').trim();
        // Case-insensitive `VISUALIZE` prefix check.
        let mut chars = trimmed.char_indices();
        let kw = "visualize";
        let head: std::string::String = trimmed.chars().take(kw.len()).collect();
        if head.to_ascii_lowercase() != kw {
            // Not ours: let the next parser extension / the core error handle it.
            return Ok(parser_dispatch::ParseOutcome::Declined);
        }
        // Advance past the keyword.
        let _ = chars.nth(kw.len().saturating_sub(1));
        let inner = trimmed[kw.len()..].trim();
        if inner.is_empty() {
            return Err(types::Duckerror::Invalidargument(
                "VISUALIZE requires a SELECT statement, e.g. VISUALIZE SELECT region, n FROM t"
                    .into(),
            ));
        }
        // Rewrite: wrap the inner select as a CTE and emit a (label, n, bar) rollup.
        // The inner select is expected to project (label, value); we render a unit
        // bar of '#' repeated by value. This desugars entirely to standard SQL --
        // the whole point of the string->SQL rewrite form.
        let rewritten = std::format!(
            "WITH __viz AS ({inner}) \
             SELECT CAST(label AS VARCHAR) AS label, \
                    CAST(n AS BIGINT) AS n, \
                    repeat('#', GREATEST(CAST(n AS BIGINT), 0)) AS bar \
             FROM (SELECT * FROM __viz) AS t(label, n) \
             ORDER BY n DESC"
        );
        Ok(parser_dispatch::ParseOutcome::Rewrite(rewritten.into()))
    }
}

// Required base export; ggsql has no scalar/table/aggregate/pragma/cast callbacks.
impl callback_dispatch::Guest for Extension {
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no scalar fns".into()))
    }
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no scalar fns".into()))
    }
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no aggregates".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no casts".into()))
    }
}

export!(Extension);
