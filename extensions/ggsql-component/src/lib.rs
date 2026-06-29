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

// VISUALIZE parse/rewrite logic now lives ONCE in the shared datalink
// `ggsql-core` crate (DB-neutral; consumed by the sqlink shim too). This
// thin ducklink shim maps the core's neutral `Outcome` onto the
// `duckdb:extension` parser-dispatch surface. The cargo-fuzz target drives
// `ggsql_core::parse_visualize` directly (the never-panic contract lives
// with the core now).
use ggsql_core::parse;

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
        // The shared, fuzzed parse/rewrite engine (datalink ggsql-core);
        // map its neutral outcome onto the WIT surface. The DuckDB dialect
        // emits `repeat`/`GREATEST`/`VARCHAR`/`BIGINT`. See ggsql-core for
        // the never-panic contract.
        match parse::parse_visualize(&query, &ggsql_core::DUCKDB) {
            parse::Outcome::Declined => Ok(parser_dispatch::ParseOutcome::Declined),
            parse::Outcome::Invalid(msg) => Err(types::Duckerror::Invalidargument(msg.into())),
            parse::Outcome::Rewrite(sql) => Ok(parser_dispatch::ParseOutcome::Rewrite(sql.into())),
        }
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
    // major-4 columnar hot path: ggsql is parser-only, so the three columnar
    // methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ggsql: no table fns".into()))
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
