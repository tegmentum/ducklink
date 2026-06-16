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

## The blocker

Rebuilding the core component against the 7-variant WIT requires recompiling
`duckdb-core-component`, which links `artifacts/libduckdb-wasi.a`. The current
archive (rebuilt 2025-11-10) is incomplete:

```
rust-lld: error: ub_duckdb_main.cpp.obj: undefined symbol: _ZTVN6duckdb8HTTPUtilE
```

`make_shared<duckdb::HTTPUtil>` is instantiated in the archive, but the
translation unit that emits `HTTPUtil`'s vtable (its key function) is not
included — so the vtable symbol is undefined. `llvm-nm artifacts/libduckdb-wasi.a`
shows `U _ZTVN6duckdb8HTTPUtilE`.

The working core wasm currently shipped in
`target/wasm32-wasip2/release/duckdb_core_component.wasm` (2025-11-08) was built
against an older, complete archive and uses the 5-variant model.

## To unblock

1. Rebuild the static archive: `scripts/build-libduckdb-wasm.sh` (ensure the TU
   defining `duckdb::HTTPUtil` is compiled in, or stop instantiating it for the
   wasm build). Confirm with
   `llvm-nm artifacts/libduckdb-wasi.a | grep _ZTVN6duckdb8HTTPUtilE` showing a
   definition (`T`/`W`), not `U`.
2. Re-apply the migration edits above.
3. Rebuild core/cli/host + sample, then re-run the end-to-end checks in
   `CURRENT_TASK.md`.
