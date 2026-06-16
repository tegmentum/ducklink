#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${DUCKDB_SOURCE_DIR:-}" ]]; then
  echo "Set DUCKDB_SOURCE_DIR to a DuckDB checkout" >&2
  exit 1
fi

if [[ -z "${WASI_SDK_PREFIX:-}" ]]; then
  echo "Set WASI_SDK_PREFIX to the wasi-sdk installation path" >&2
  exit 1
fi

WASM_EXTENSIONS=${WASM_EXTENSIONS:-"json"}

BUILD_DIR=${BUILD_DIR:-"$(pwd)/build/duckdb-wasi"}
mkdir -p "$BUILD_DIR"

echo "Configuring DuckDB for wasm32-wasi in $BUILD_DIR" >&2
env WASM_EXTENSIONS="$WASM_EXTENSIONS" cmake -S "$DUCKDB_SOURCE_DIR" -B "$BUILD_DIR" \
  -DCMAKE_TOOLCHAIN_FILE="$(pwd)/cmake/toolchains/wasi-sdk.cmake" \
  -DWASI_SDK_PREFIX:PATH="$WASI_SDK_PREFIX" \
  -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
  -DBUILD_SHELL=OFF \
  -DBUILD_TESTS=OFF \
  -DBUILD_BENCHMARK=OFF \
  -DDUCKDB_PLATFORM="wasm32-wasi" \
  -DDUCKDB_LIBDYNAMIC=OFF \
  -DDUCKDB_LIBDUCKDB_STATIC=ON

echo "Building libduckdb static archive" >&2
cmake --build "$BUILD_DIR" --target duckdb_static

STATIC_LIB="$(find "$BUILD_DIR" -name 'libduckdb_static.a' -print -quit)"
if [[ -z "$STATIC_LIB" ]]; then
  echo "libduckdb_static.a not found; check the build output" >&2
  exit 1
fi

ARTIFACTS_DIR=${ARTIFACTS_DIR:-"$(pwd)/artifacts"}
mkdir -p "$ARTIFACTS_DIR"
# Merge DuckDB with the C++ runtime archives so downstream consumers
# do not need to manually link libc++/libc++abi when building components. Use
# the `eh` multilib (exception-handling) variants plus libunwind so the merged
# archive carries the runtime that DuckDB's `-fwasm-exceptions` code needs.
SYSROOT_LIBDIR="$WASI_SDK_PREFIX/share/wasi-sysroot/lib/${WASI_TARGET_TRIPLE:-wasm32-wasip1-threads}/eh"
if [[ ! -d "$SYSROOT_LIBDIR" ]]; then
  echo "Expected exception-handling sysroot lib directory '$SYSROOT_LIBDIR' not found (needs wasi-sdk >= 33)" >&2
  exit 1
fi

TMPDIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMPDIR"
}
trap cleanup EXIT

cp "$STATIC_LIB" "$TMPDIR/libduckdb_base.a"
cp "$SYSROOT_LIBDIR/libc++abi.a" "$TMPDIR/libc++abi.a"
cp "$SYSROOT_LIBDIR/libc++.a" "$TMPDIR/libc++.a"
cp "$SYSROOT_LIBDIR/libunwind.a" "$TMPDIR/libunwind.a"
pushd "$TMPDIR" >/dev/null
cat <<EOF | "$WASI_SDK_PREFIX/bin/llvm-ar" -M
CREATE libduckdb_combined.a
ADDLIB libduckdb_base.a
ADDLIB libc++abi.a
ADDLIB libc++.a
ADDLIB libunwind.a
SAVE
END
EOF
popd >/dev/null

cp "$TMPDIR/libduckdb_combined.a" "$ARTIFACTS_DIR/libduckdb-wasi.a"

echo "Static library copied to $ARTIFACTS_DIR/libduckdb-wasi.a" >&2
