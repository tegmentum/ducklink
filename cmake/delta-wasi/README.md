# Delta extension on wasm — build-out recipe

Feasibility for DuckDB's `delta` extension on wasm32-wasip2 is **proven**; this
directory holds the enabling patches. The remaining step is the libduckdb build
(its next blocker is documented below). `aws` is unnecessary (httpfs already does
S3+sigv4 with `CREATE SECRET (TYPE s3)`); `azure` is SDK-C++-blocked (use
SAS-token URLs via httpfs).

## What's proven

1. **delta-kernel-rs compiles to wasm32-wasip2** (sync engine = local files, no
   tokio/reqwest/object_store). 35.5 MB `libdelta_kernel_ffi.a`.
2. **It links** with the wasi-sdk C/C++ toolchain (wasm-ld resolves the full
   scan/arrow/parquet path; no undefined symbols).
3. **The sync engine is FFI-constructible** — the kernel FFI only exposed cloud
   (`get_default_engine`) constructors; `get_sync_engine.patch` adds a
   `get_sync_engine` (mirrors `get_default_engine` over `SyncEngine::new()`),
   which compiles, exports, and appears in the cbindgen header.

## Build recipe

Kernel (commit 08f0764 = the in-tree `extension/delta` kernel):
```
git apply cmake/delta-wasi/get_sync_engine.patch          # add get_sync_engine FFI
cargo update -p chrono --precise 0.4.39                     # chrono 0.4.45 breaks arrow-arith 51
CC_wasm32-wasip2 / CC_wasm32_wasip2 / AR_* = <wasi-sdk>/bin/{clang,llvm-ar}
CFLAGS_wasm32-wasip2=--sysroot=<wasi-sdk>/share/wasi-sysroot
cargo build -p delta_kernel_ffi --no-default-features --features sync-engine \
  --target wasm32-wasip2 --release
```

C++ extension (`delta_scan_sync_engine.patch` on
`extension/delta/src/functions/delta_scan.cpp`): under `__wasi__`, construct the
engine via `ffi::get_sync_engine(...)` instead of `CreateBuilder` +
`builder_build` (the cloud builder API isn't compiled into the sync kernel), and
`#ifndef __wasi__`-guard `CreateBuilder`.

Then patch the delta CMakeLists (`RUST_PLATFORM_TARGET=wasm32-wasip2`,
`--no-default-features --features sync-engine`), add `duckdb_extension_load(delta)`
to `cmake/wasm-extension-config.cmake`, and rebuild libduckdb + the core component.

## Next blocker (the remaining work)

**Duplicate C symbols at the libduckdb/core link**: the kernel `.a` bundles its
own zstd (defines `ZSTD_compress`/`ZSTD_decompress`) and lz4, which collide with
DuckDB's own C++ zstd/lz4 (and the curl-wasm zstd already merged for httpfs).
Resolve via one of: `wasm-ld --allow-multiple-definition`, building the kernel's
parquet without the zstd codec, or objcopy-localizing the kernel's zstd symbols.

## Costs / scope

- **Local Delta only** (sync engine = `std::fs`). Remote (`s3://`) needs routing
  delta-kernel's I/O through DuckDB's FileSystem/httpfs (the kernel uses its own
  object_store, whose reqwest/tokio has no wasip2 transport).
- **~35 MB bloat**: the kernel duplicates Parquet/Arrow/zstd that DuckDB already
  has in C++ (~96 MB core -> ~130 MB).
