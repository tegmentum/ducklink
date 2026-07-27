# dplyr-component

Parser extension that transpiles `dplyr( tbl |> verb(..) |> .. )`
statements into ordinary DuckDB SQL. The transpiler itself lives in the
DB-neutral `dplyr-core` crate; this shim maps its neutral `Outcome` onto
the parser-dispatch surface.

## @5 status: dispatch surface is silently no-op'd

At `duckdb:extension@5.0.0` the `parser` interface (and its
`parser-dispatch` export) is DEPRECATED and no host consumes it:

- `crates/ducklink-runtime/src/extension.rs` still implements
  `extension_parser::Host::register_parser_extension` — the call succeeds
  and the registration is captured into a `pending_parsers` buffer — but
  the buffer is never drained anymore. The core shim that used to wire a
  DuckDB `ParserExtension` around the captured extensions is gone at @5.
- DuckDB's `ParserExtension` sits on the unstable internal C++ ABI with no
  equivalent in the stable C API, so there is no C-API path to re-home
  the dispatch to. This is the "no C-API equivalent" bucket in the @5 ADR
  (Decision 5(b)).

Net effect: `LOAD dplyr;` succeeds and `dplyr.load()` reports success, but
`dplyr(...)` statements are handed straight to the built-in parser, which
rejects them with a plain syntax error. No rewrite ever fires.

Users needing dplyr syntax against DuckDB should either invoke the
`dplyr-core` transpiler out-of-band (produce SQL, then run it) or wait
for a stable-C-API parser-extension hook to land.

## Build

```
cargo component build -p dplyr-component --release --target wasm32-wasip2
```

Artifact: `target/wasm32-wasip1/release/dplyr.wasm`.
