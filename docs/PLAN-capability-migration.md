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

## Follow-up — forward catalog/files registrations into DuckDB

The host's `catalog`/`files` Host impls accept and log requests but do not yet
register anything in DuckDB. Completing them means, mirroring the
scalar/table/aggregate pipeline: capture each request into per-store pending
buffers, extend `extension-loader-hooks` (core WIT) with catalog/files
registration records, drain them in `duckdb_component_load_extension`, and call
the DuckDB C API to create logical types / casts / macros / replacement scans /
copy handlers. A using-extension + test should land with that work (the current
`sample-extension-component` does not exercise `catalog`/`files`).

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
