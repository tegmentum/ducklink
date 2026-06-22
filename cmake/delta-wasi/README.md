# Delta extension on wasm — WORKING (local read)

DuckDB's `delta` extension reads **local** Delta Lake tables on wasm32-wasip2:
`SELECT * FROM delta_scan('<local path>')` runs end-to-end through the wasm core
(verified against `test/fixtures/delta_people`, matches native DuckDB). It wraps
delta-kernel-rs **v0.14.0**'s SYNC engine (local `std::fs`; no
tokio/reqwest/object_store). `aws` is unnecessary (httpfs already does S3+sigv4
with `CREATE SECRET (TYPE s3)`); `azure` is SDK-C++-blocked (SAS-token URLs via
httpfs).

NOTE: the project's vendored `external/duckdb/extension/delta` is the
**version-matched** duckdb-delta @ `fa847248` (DuckDB's own
`.github/config/extensions/delta.cmake` pins it; it already targets this DuckDB's
MultiFileReader API). DuckDB upstream EXCLUDES delta on wasm (`NOT
${WASM_ENABLED}`), so this sync-engine path is novel.

## How it's wired

- **`scripts/build-delta-kernel-wasm.sh`** — reproducible kernel build: clones
  delta-kernel-rs `v0.14.0` (delta-io repo), applies
  `kernel-v0.14.0-sync-engine.patch`, builds `libdelta_kernel_ffi.a` for
  wasm32-wasip2 (features `sync-engine,tracing,test-ffi`) with the wasi-sdk C
  toolchain, stages it under `build/delta-kernel/out/`.
- **`scripts/build-libduckdb-wasm.sh`** (`stage_delta_kernel`) — stages that `.a`
  where the (patched, vendored) delta CMakeLists expects it, and merges the
  kernel into `libduckdb-wasi.a`.
- **`crates/ducklink-core/build.rs`** — adds
  `--allow-multiple-definition` to the core link when the lib contains the kernel
  (the kernel is a Rust `staticlib` bundling its own std runtime, which collides
  with the core's std; same toolchain, so the linker keeps the first copy).
- **`cmake/wasm-extension-config.cmake`** — `duckdb_extension_load(delta
  SOURCE_DIR ...)`, guarded on the staged kernel.

## The kernel patch (`kernel-v0.14.0-sync-engine.patch`)

v0.14.0 demoted the sync engine to a test-only, crate-private module and has NO
FFI sync-engine feature, so the patch:
- adds a kernel `sync-engine` feature (arrow + object_store WITHOUT cloud
  backends; parquet WITHOUT the zstd/brotli C codecs that collide with DuckDB's
  zstd + curl-wasm's libbrotli),
- re-exposes the sync module + makes `SyncEngine`/`new()` public,
- un-gates the arrow/parquet `Error` variants + the `engine_data` FFI
  (`need-arrow` / `sync-engine`) and the `ExternEngineVtable`/`engine_to_handle`
  plumbing so a sync engine can be wrapped,
- adds the `get_sync_engine` FFI constructor.

## The delta C++ patches (`delta-fa847248-*.patch`)

- **delta_kernel_ffi.hpp** — declares `get_sync_engine` under `DEFINE_SYNC_ENGINE`.
- **delta_multi_file_list.cpp** — `InitializeSnapshot` under `__wasi__` builds the
  engine via `get_sync_engine` instead of `CreateBuilder`+`builder_build`;
  `#ifndef __wasi__`-guards `CreateBuilder` (the whole cloud option-setting fn).
- **delta_utils.cpp** — `#if defined(DEFINE_DEFAULT_ENGINE_BASE)`-guards the
  arrow/parquet/object_store/reqwest entries of `KERNEL_ERROR_ENUM_STRINGS` so
  the enum numbering + `static_assert` match the sync-ABI kernel.
- **CMakeLists.txt** — `RUST_PLATFORM_TARGET=wasm32-wasip2`, no-op the kernel
  ExternalProject (prebuilt + staged), `add_compile_definitions(DEFINE_SYNC_ENGINE)`.

## A wasi-fs shim fix (not delta-specific)

Delta is the first extension to list a non-empty directory through the core's
`__wrap_readdir`. That shim sized the name buffer from `libc::dirent.d_name`,
which is a zero-length flexible-array member on wasm (`cap=0`) -> every entry hit
`ENAMETOOLONG`. Fixed in `crates/ducklink-core/src/lib.rs` by backing the
dirent with an over-sized buffer and writing the name at the `d_name` offset.

## Scope / costs

- **Local Delta only** (sync engine = `std::fs`). Remote (`s3://`) needs routing
  delta-kernel's I/O through DuckDB's FileSystem/httpfs (the kernel uses its own
  object_store, whose reqwest/tokio has no wasip2 transport).
- **~50 MB** added (`libdelta_kernel_ffi.a`): the kernel bundles its own Rust
  arrow/parquet, duplicating DuckDB's C++ ones.
- Snappy/gzip/lz4-compressed Delta read; **zstd/brotli-compressed Delta won't**
  (codecs dropped). Delta defaults to snappy, so this is rarely a constraint.
