#!/usr/bin/env bash
# Build the Azure SDK for C++ (the subset the `azure` extension needs) for
# wasm32-wasip2: azure-core (with the libcurl HTTP transport), azure-storage-common,
# azure-storage-blobs, azure-storage-files-datalake, azure-identity. Compiled
# directly (no vcpkg/CMake) against the already-wasm-built curl-wasm + openssl-wasm
# + libxml2-wasm. Transport is libcurl over wasi:sockets -- exactly how httpfs works.
#
# Output: $OUT_DIR/lib/lib{azure-core,azure-storage-common,azure-storage-blobs,
# azure-storage-files-datalake,azure-identity}.a + a copy of the include trees.
# Env: WASI_SDK_PREFIX (required), AZURE_SDK_SRC (the azure-sdk-for-cpp checkout),
#      CURL_WASM / OPENSSL_WASM / LIBXML2_WASM (dep roots), OUT_DIR.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WASI_SDK_PREFIX="${WASI_SDK_PREFIX:?set WASI_SDK_PREFIX}"
AZURE_SDK_SRC="${AZURE_SDK_SRC:-$ROOT/build/azure-sdk-for-cpp}"
AZURE_SDK_COMMIT="${AZURE_SDK_COMMIT:-e9f2fa39b73496fd6a33d1e6b4a5c1cf325b3a2c}"  # pinned (compiles the ext)
CURL_WASM="${CURL_WASM:-$HOME/git/curl-wasm/build}"
OPENSSL_WASM="${OPENSSL_WASM:-$HOME/git/openssl-wasm}"
LIBXML2_WASM="${LIBXML2_WASM:-$HOME/git/libxml2-wasm/build/install}"
OUT_DIR="${OUT_DIR:-$ROOT/build/azure-sdk/out}"

CXX="$WASI_SDK_PREFIX/bin/clang++"
AR="$WASI_SDK_PREFIX/bin/llvm-ar"
SYSROOT="$WASI_SDK_PREFIX/share/wasi-sysroot"

if [[ ! -d "$AZURE_SDK_SRC/.git" ]]; then
  echo ">> cloning azure-sdk-for-cpp @ $AZURE_SDK_COMMIT" >&2
  git clone --quiet https://github.com/Azure/azure-sdk-for-cpp "$AZURE_SDK_SRC"
  git -C "$AZURE_SDK_SRC" checkout --quiet "$AZURE_SDK_COMMIT"
fi
# Treat wasm as a POSIX platform (AZ_PLATFORM_POSIX) -- idempotent.
if ! grep -q '__wasi__' "$AZURE_SDK_SRC/sdk/core/azure-core/inc/azure/core/platform.hpp"; then
  git -C "$AZURE_SDK_SRC" apply "$ROOT/cmake/azure-deps/azure-sdk-platform-wasi.patch"
  echo ">> applied AZ_PLATFORM_POSIX wasm patch" >&2
fi

INCS=(
  -I "$AZURE_SDK_SRC/sdk/core/azure-core/inc"
  -I "$AZURE_SDK_SRC/sdk/storage/azure-storage-common/inc"
  -I "$AZURE_SDK_SRC/sdk/storage/azure-storage-blobs/inc"
  -I "$AZURE_SDK_SRC/sdk/storage/azure-storage-files-datalake/inc"
  -I "$AZURE_SDK_SRC/sdk/identity/azure-identity/inc"
  -I "$CURL_WASM/curl/include"
  -I "$OPENSSL_WASM/build/openssl/include"
  -I "$OPENSSL_WASM/third_party/openssl/include"
  -I "$LIBXML2_WASM/include/libxml2"
  # stub <spawn.h>/<sys/wait.h> so AzureCliCredential compiles (no wasm subprocess)
  -I "$ROOT/cmake/azure-deps/wasm-stubs"
)

CXXFLAGS=(
  --target=wasm32-wasip2 --sysroot="$SYSROOT"
  -std=c++17 -fexceptions -O2 -fPIC
  -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS
  -DBUILD_CURL_HTTP_TRANSPORT_ADAPTER  # default transport = CurlTransport
  -DCURL_STATICLIB
)

mkdir -p "$OUT_DIR/lib" "$OUT_DIR/obj"

# Compile one library: $1=name, rest=source dirs (relative to the lib root).
build_lib() {
  local name="$1"; shift
  local libroot="$1"; shift
  local objdir="$OUT_DIR/obj/$name"
  mkdir -p "$objdir"
  local objs=()
  # all .cpp under src/, excluding the Windows winhttp transport
  while IFS= read -r src; do
    case "$src" in *winhttp*) continue;; esac
    local o="$objdir/$(echo "$src" | sed "s#$libroot/##; s#/#_#g; s#\.cpp#.o#")"
    "$CXX" "${CXXFLAGS[@]}" "${INCS[@]}" -c "$src" -o "$o"
    objs+=("$o")
  done < <(find "$libroot/src" -name '*.cpp')
  "$AR" rcs "$OUT_DIR/lib/lib${name}.a" "${objs[@]}"
  echo ">> built lib${name}.a ($(echo "${#objs[@]}") objs)" >&2
}

build_lib azure-core                       "$AZURE_SDK_SRC/sdk/core/azure-core"
build_lib azure-storage-common             "$AZURE_SDK_SRC/sdk/storage/azure-storage-common"
build_lib azure-storage-blobs              "$AZURE_SDK_SRC/sdk/storage/azure-storage-blobs"
build_lib azure-storage-files-datalake     "$AZURE_SDK_SRC/sdk/storage/azure-storage-files-datalake"
# azure-identity also gets the subprocess stubs (posix_spawn/waitpid/kill/pipe ->
# ENOSYS) so AzureCliCredential links; it's unavailable at runtime, others work.
"$WASI_SDK_PREFIX/bin/clang" --target=wasm32-wasip2 --sysroot="$SYSROOT" -O2 -fPIC \
  -I "$ROOT/cmake/azure-deps/wasm-stubs" \
  -c "$ROOT/cmake/azure-deps/wasm-stubs/azure_subprocess_stubs.c" \
  -o "$OUT_DIR/obj/azure_subprocess_stubs.o"
build_lib azure-identity                   "$AZURE_SDK_SRC/sdk/identity/azure-identity"
"$AR" rs "$OUT_DIR/lib/libazure-identity.a" "$OUT_DIR/obj/azure_subprocess_stubs.o"
echo ">> added subprocess stubs to libazure-identity.a" >&2

# Stage include trees for the extension build.
mkdir -p "$OUT_DIR/include"
for d in core/azure-core storage/azure-storage-common storage/azure-storage-blobs \
         storage/azure-storage-files-datalake identity/azure-identity; do
  cp -R "$AZURE_SDK_SRC/sdk/$d/inc/." "$OUT_DIR/include/"
done
echo ">> done: $OUT_DIR/lib + $OUT_DIR/include" >&2
