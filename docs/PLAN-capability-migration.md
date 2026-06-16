# PLAN — capability-kind migration (5 -> 7) and catalog/files interfaces

Parked work. The repo was mid-migration from 5 to 7 `capabilitykind` variants
and adding two new extension-facing interfaces. It is **blocked on a DuckDB
static-archive rebuild**, so it was reverted to keep the loader working on the
existing core wasm. This doc captures what to re-apply once the archive is
rebuilt.

## What the migration adds

- `wit/duckdb-extension/types.wit`: extend `enum capabilitykind` with
  `catalog` and `file-format`.
- `wit/duckdb-extension/catalog.wit`: `interface catalog`
  (`register-logical-type`, `register-cast`, `register-macro`). Already rewritten
  into valid WIT; currently present but unreferenced by the world.
- `wit/duckdb-extension/files.wit`: `interface files`
  (`register-replacement-scan`, `register-copy-handler`). Same status.
- `wit/duckdb-extension/worlds/duckdb-extension.wit`: re-add
  `use catalog; use files;` and `import catalog; import files;`.
- Rust: re-add the `Catalog` / `FileFormat` match arms in
  `crates/duckdb-component-host/src/lib.rs` (`convert_core_capabilitykind`,
  `convert_cli_capability`, `describe_cli_capability`) and in
  `crates/duckdb-core-component/src/extension_loader.rs` (`describe_capability`).
- Host: implement `catalog::Host` + `files::Host` for `ExtensionStoreState` and
  add them to the extension linker in `ensure_extension_loaded`, mirroring the
  scalar/table/aggregate registry pattern (queue on register, retain on drop,
  forward in `drain_pending`). Without this, any extension built against the new
  world imports `catalog`/`files` and fails to instantiate (the host must
  satisfy every world import).

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
