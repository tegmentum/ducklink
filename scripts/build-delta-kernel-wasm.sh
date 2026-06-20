#!/usr/bin/env bash
# Build delta-kernel-rs's FFI staticlib for wasm32-wasip2 (the SYNC engine only:
# local std::fs, no tokio/reqwest/object_store). Produces a libdelta_kernel_ffi.a
# that links into libduckdb-wasi.a for the vendored `delta` extension
# (duckdb-delta @ fa847248, which pins kernel v0.14.0).
#
# v0.14.0 demoted the sync engine to a test-only, crate-private module and has no
# FFI sync-engine feature, so the patch below is substantial -- it:
#   - adds a kernel `sync-engine` feature (arrow, no object_store cloud backends),
#   - re-exposes the sync module + makes SyncEngine/new() public,
#   - un-gates the arrow/parquet error variants (need-arrow) + the FFI
#     ExternEngineVtable/engine_to_handle (so a sync engine can be wrapped),
#   - adds the `get_sync_engine` FFI constructor,
#   - drops the parquet zstd+brotli codecs (their bundled C symbols collide with
#     DuckDB's zstd + curl-wasm's libbrotli) and object_store's cloud backends.
# See cmake/delta-wasi/kernel-v0.14.0-sync-engine.patch.
#
# Output: $OUT_DIR/libdelta_kernel_ffi.a + $OUT_DIR/ffi-headers/.
# Env: WASI_SDK_PREFIX (required), KERNEL_SRC (work dir), OUT_DIR.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WASI_SDK_PREFIX="${WASI_SDK_PREFIX:?set WASI_SDK_PREFIX to the wasi-sdk root}"
KERNEL_TAG="v0.14.0"   # matches duckdb-delta @ fa847248 (the vendored extension)
KERNEL_SRC="${KERNEL_SRC:-$ROOT/build/delta-kernel/src}"
OUT_DIR="${OUT_DIR:-$ROOT/build/delta-kernel/out}"
PATCH="$ROOT/cmake/delta-wasi/kernel-v0.14.0-sync-engine.patch"

if [[ ! -d "$KERNEL_SRC/.git" ]]; then
  echo ">> cloning delta-kernel-rs @ $KERNEL_TAG" >&2
  git clone --depth 1 --branch "$KERNEL_TAG" \
    https://github.com/delta-io/delta-kernel-rs "$KERNEL_SRC"
fi

echo ">> applying sync-engine FFI patch (idempotent)" >&2
git -C "$KERNEL_SRC" checkout -- . 2>/dev/null || true
git -C "$KERNEL_SRC" apply "$PATCH"

echo ">> building delta_kernel_ffi (sync-engine, release, wasm32-wasip2)" >&2
( cd "$KERNEL_SRC" && env \
    "CC_wasm32_wasip2=$WASI_SDK_PREFIX/bin/clang" \
    "CC_wasm32-wasip2=$WASI_SDK_PREFIX/bin/clang" \
    "AR_wasm32_wasip2=$WASI_SDK_PREFIX/bin/llvm-ar" \
    "AR_wasm32-wasip2=$WASI_SDK_PREFIX/bin/llvm-ar" \
    "CFLAGS_wasm32_wasip2=--sysroot=$WASI_SDK_PREFIX/share/wasi-sysroot" \
    "CFLAGS_wasm32-wasip2=--sysroot=$WASI_SDK_PREFIX/share/wasi-sysroot" \
    cargo build -p delta_kernel_ffi --no-default-features --features sync-engine,tracing,test-ffi \
      --target wasm32-wasip2 --release )

mkdir -p "$OUT_DIR/ffi-headers"
cp "$KERNEL_SRC/target/wasm32-wasip2/release/libdelta_kernel_ffi.a" "$OUT_DIR/"
# the delta extension uses its committed header, but stage the generated ones too
find "$KERNEL_SRC/target" -name 'delta_kernel_ffi.h*' -path '*ffi-headers*' \
  -exec cp {} "$OUT_DIR/ffi-headers/" \; 2>/dev/null || true
echo ">> done: $OUT_DIR/libdelta_kernel_ffi.a" >&2
