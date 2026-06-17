# PLAN — capability-kind migration (5 -> 7) and catalog/files interfaces

Status (2026-06): **applied.** The capability model is now 7-variant and the
`catalog`/`files` interfaces are part of the extension world, satisfied by the
host. Verified end-to-end (sample scalar/table/aggregate) plus the host test
suite. Remaining work is the DuckDB-side wiring of catalog/files registrations
(see "Follow-up" below).

## What the migration added (done)

- `wit/duckdb-extension/types.wit`: `enum capabilitykind` now includes
  `catalog` and `file-format` (7 variants), synced to all crates and the sample.
- `wit/duckdb-extension/catalog.wit`: `interface catalog`
  (`register-logical-type`, `register-cast`, `register-macro`).
- `wit/duckdb-extension/files.wit`: `interface files`
  (`register-replacement-scan`, `register-copy-handler`).
- `wit/duckdb-extension/worlds/duckdb-extension.wit`: imports `catalog`/`files`
  (also mirrored into the sample extension's world).
- Rust: `Catalog`/`FileFormat` match arms restored in
  `crates/duckdb-component-host/src/lib.rs` (`convert_core_capabilitykind`,
  `convert_cli_capability`, `describe_cli_capability`) and in
  `crates/duckdb-core-component/src/extension_loader.rs` (`describe_capability`).
- Host: `extension_catalog::Host` + `extension_files::Host` implemented for
  `ExtensionStoreState` and added to the extension linker in
  `ensure_extension_loaded`, so extensions that import `catalog`/`files`
  instantiate. The host currently acknowledges and logs each registration.

## Forwarding catalog/files registrations into DuckDB

What the DuckDB C API actually supports (surveyed against `external/duckdb`):

| Registration   | C API path | Status |
|----------------|-----------|--------|
| macro          | none — `CREATE MACRO` SQL only | **working** (see below) |
| replacement scan | `duckdb_add_replacement_scan` | **working** (see below) |
| logical type   | no named-type registration | not feasible as specified |
| cast           | `duckdb_create_cast_function` needs a callback; WIT `cast-spec` carries none | not feasible as specified |
| copy handler   | none | not feasible |

### Macros — WORKING (2026-06)

The macro path works end-to-end on wasi-sdk-33 with exception handling enabled:
`sample-extension-component` calls `catalog.register-macro`; the host captures it
(`ExtensionStoreState.pending_macros`), drains and forwards it through
`extension-loader-hooks` (`macro-registration` record + `pending-registrations.macros`);
the core (`register_pending_macro`, gated by `MACRO_EXECUTION_ENABLED = true`)
builds `CREATE OR REPLACE MACRO …` and runs it on a **transient connection** to
each active database (never the LOAD-busy connection). Verified:
`select sample_add_two(40)` → 42 (`cli_executes_sample_macro` test).

This required enabling wasm C++ exceptions — see the SCOPE section, now done.

### Replacement scans — WORKING (2026-06)

`sample-extension-component` registers a `sample_read_path(VARCHAR)` table
function and calls `files.register-replacement-scan({extensions: ["sample"],
table-function: <handle>, …})`. The host resolves the table-function handle to
its name (`table_handle_names` map) and forwards a
`replacement-scan-registration` through `extension-loader-hooks`. The core
(`register_pending_replacement_scan`) stores the spec and installs one global
`duckdb_add_replacement_scan` callback per database; the callback rewrites
`FROM 'file.ext'` to the registered table function, passing the name as its
argument. Verified: `select * from 'hello.sample'` → row `hello.sample`
(`cli_executes_replacement_scan` test).

The required `duckdb_add_replacement_scan` / `duckdb_replacement_scan_*` /
`duckdb_create_varchar` C-API entries were added to the curated
`crates/libduckdb-sys` FFI bindings.

## wasm exception handling — RESOLVED via wasi-sdk-33 (2026-06)

Goal: make C++ `throw`/`catch` actually unwind (caught -> error) instead of
`__cxa_throw -> std::terminate -> abort`. Payoff is large: it unblocked macros
**and** turned every DuckDB SQL error from a fatal module abort into a
recoverable error (`SELECT * FROM nonexistent` now returns a Catalog Error).

wasi-sdk-28's bundled libc++ was `-fno-exceptions` (no exception runtime), so
this was not a flag flip. **wasi-sdk-33 ships an exception-handling `eh`
multilib** (`share/wasi-sysroot/lib/wasm32-wasip2/eh/{libc++,libc++abi,libunwind}.a`,
built with the standardized `try_table`/`throw_ref` encoding). Switching to it
made the path mechanical. What was done:

1. Toolchain (`cmake/toolchains/wasi-sdk.cmake`): add
   `-fwasm-exceptions -mllvm -wasm-use-legacy-eh=false` to CXX flags — the
   `-wasm-use-legacy-eh=false` is required because clang's default
   `-fwasm-exceptions` still emits the *legacy* encoding, which wasmtime's
   production `exceptions` feature rejects; the standardized encoding matches
   wasi-sdk-33's `eh` libs.
2. `scripts/build-libduckdb-wasm.sh`: merge the `eh` libc++/libc++abi + libunwind
   into `libduckdb-wasi.a`.
3. `crates/libduckdb-sys/build.rs`: stop separately linking c++abi/c++ (now baked
   in the archive) to avoid duplicate `__cxa_*`; link only `m`.
4. `crates/duckdb-core-component/src/lib.rs`: remove the old aborting `__cxa_*`
   stubs (the real EH libc++abi now provides them).
5. Host `build_engine`: `config.wasm_exceptions(true)`.
6. `MACRO_EXECUTION_ENABLED = true`.

Also required for the toolchain bump (clang 21 -> 22): a one-line patch to
DuckDB's bundled thrift (`third_party/thrift/thrift/Thrift.h`) adding
`TEnumIterator::operator==` (newer libc++ requires equality-comparable
iterators), and registering extension scalar/table/aggregate functions on a
**transient connection** per database rather than the LOAD-busy active
connection (which now surfaces a real error instead of being silently tolerated).

## The archive blocker — RESOLVED (2026-06)

Rebuilding the core component requires recompiling `duckdb-core-component`,
which links `artifacts/libduckdb-wasi.a`. The 2025-11-10 archive was incomplete:

```
rust-lld: error: ub_duckdb_main.cpp.obj: undefined symbol: _ZTVN6duckdb8HTTPUtilE
```

Root cause: the WASI toolchain sets `DUCKDB_SKIP_HTTP ON`
(`cmake/toolchains/wasi-sdk.cmake`), which excludes `src/main/http/http_util.cpp`
(the TU that emits `HTTPUtil`'s vtable) — but core DuckDB still constructed an
`HTTPUtil` at `src/main/database.cpp:53` (`http_util = make_shared_ptr<HTTPUtil>()`),
referencing the now-missing vtable.

Fix applied:
- `cmake/toolchains/wasi-sdk.cmake`: add `-DDUCKDB_SKIP_HTTP` to the C/CXX flags.
- `external/duckdb/src/main/database.cpp`: guard the construction with
  `#ifndef DUCKDB_SKIP_HTTP` so `http_util` stays null when HTTP is skipped.
- `crates/libduckdb-sys/build.rs`: add `rerun-if-changed`/`rerun-if-env-changed`
  on `DUCKDB_STATIC_LIB` so cargo stops bundling a stale archive into the rlib.

The archive now rebuilds cleanly (`llvm-nm` shows no `_ZTVN6duckdb8HTTPUtilE`
reference) and `duckdb-core-component` links and runs. A second issue surfaced
and was fixed at the same time: the core is a reactor component whose host does
not wire `wasi:cli/stderr`, so std `eprintln!` aborted DuckDB mid-load — core
logging now goes through a non-panicking `clog!` macro (see `src/lib.rs`).

So the 5->7 migration is **no longer blocked**. Remaining work is just the
re-apply steps:

1. Re-apply the migration edits listed above (enum, world imports, Rust arms,
   host `catalog`/`files` Host impls).
2. Rebuild core/cli/host + sample, then re-run the end-to-end checks in
   `CURRENT_TASK.md`.

Note: the core still does not actually import `wasi:cli/stderr`/`stdout` (the
reactor adapter in the current `cargo-component` does not wire stdio even with
the world imports added). That only affects visibility of the core's debug
logs, not functionality. Restoring real core stdio is a separate follow-up.
