//! `prql` TRANSPARENT parser extension — ducklink (`duckdb:extension`) port.
//!
//! The host wires a DuckDB `ParserExtension` that forwards a statement the
//! built-in parser rejected to `parser-dispatch.call-parse`; this
//! component recognizes a bare PRQL pipeline (`from x | filter .. |
//! select ..`) and compiles it to ordinary DuckDB SQL the core runs in its
//! place. No bound parse tree crosses the WIT boundary -- text in, SQL text
//! out (the by-value-safe form, see wit/parser-dispatch.wit).
//!
//! This is the transparent upgrade of the explicit `prql_to_sql(prql)`
//! scalar: no function call is needed; the PRQL just runs. The compile
//! (prqlc, dialect-parameterized) lives ONCE in the shared datalink
//! `prql-core` crate (the sqlink shim consumes the same core); this thin
//! shim maps the core's neutral `Outcome` onto the parser-dispatch surface,
//! emitting the DuckDB dialect.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

use prql_core::parse;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-parser" });

use duckdb::extension::{parser, types};
use exports::duckdb::extension::{callback_dispatch, guest, parser_dispatch};

/// Opaque handle the host threads back into call-parse.
const PARSER_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        parser::register_parser_extension("prql", PARSER_HANDLE)?;
        Ok(types::Loadresult {
            name: "prql".into(),
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
        // The shared PRQL compiler (datalink prql-core); map its neutral
        // outcome onto the WIT surface, targeting the DuckDB dialect. See
        // prql-core for the never-panic contract (prqlc is catch_unwind'd).
        match parse::parse_prql(&query, prql_core::DUCKDB) {
            parse::Outcome::Declined => Ok(parser_dispatch::ParseOutcome::Declined),
            parse::Outcome::Invalid(msg) => Err(types::Duckerror::Invalidargument(msg.into())),
            parse::Outcome::Rewrite(sql) => Ok(parser_dispatch::ParseOutcome::Rewrite(sql.into())),
        }
    }
}

// Required base export; prql parser has no scalar/table/aggregate/pragma/cast.
impl callback_dispatch::Guest for Extension {
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("prql: no scalar fns".into()))
    }
    // major-4 columnar dispatch: prql parser is a parser-only component, so the
    // three columnar hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("prql: no table fns".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("prql: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("prql: no casts".into()))
    }
}

export!(Extension);
