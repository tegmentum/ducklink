#!/usr/bin/env bash
# Build the C libraries the avro + iceberg extensions need, for wasm32-wasi:
#   - jansson  (JSON)        -> avro-c
#   - avro-c   (Apache Avro) -> the `avro` extension's read_avro (deflate codec
#                               only: snappy/lzma are disabled, so no extra libs)
#   - roaring  (CRoaring)    -> linked directly into iceberg
# Outputs static libs + headers (+ cmake config for roaring) under build/wasi-deps/.
# Idempotent: re-run to rebuild. Requires WASI_SDK_PREFIX and the wasi toolchain
# file. zlib (deflate) is reused from ~/git/curl-wasm.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${WASI_SDK_PREFIX:?set WASI_SDK_PREFIX to the wasi-sdk install}"
TOOLCHAIN="$ROOT/cmake/toolchains/wasi-sdk.cmake"
DEPS="$ROOT/build/wasi-deps"
SRC="$DEPS/src"
ZLIB="${ZLIB_WASM_DIR:-$HOME/git/curl-wasm/build/zlib}"
mkdir -p "$SRC"

NM="$WASI_SDK_PREFIX/bin/llvm-nm"
cmake_wasi() { cmake -S "$1" -B "$2" \
  -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN" -DWASI_SDK_PREFIX="$WASI_SDK_PREFIX" \
  -DBUILD_SHARED_LIBS=OFF "${@:3}"; }

# --- jansson ---------------------------------------------------------------
if [[ ! -d "$SRC/jansson" ]]; then
  git clone --depth 1 https://github.com/akheron/jansson "$SRC/jansson"
fi
cmake_wasi "$SRC/jansson" "$SRC/jansson/build-wasi" \
  -DCMAKE_INSTALL_PREFIX="$DEPS/jansson" \
  -DJANSSON_BUILD_DOCS=OFF -DJANSSON_EXAMPLES=OFF -DJANSSON_WITHOUT_TESTS=ON \
  -DJANSSON_BUILD_SHARED_LIBS=OFF
cmake --build "$SRC/jansson/build-wasi" --target install
echo "[deps] jansson -> $DEPS/jansson/lib/libjansson.a" >&2

# --- snappy ----------------------------------------------------------------
# avro-c's snappy codec (snappy-c.h). Source is bundled in ~/git/snappy-wasm;
# installs a SnappyConfig.cmake that avro-c's find_package(Snappy CONFIG) finds.
if [[ ! -d "$SRC/snappy" ]]; then
  if [[ -d "$HOME/git/snappy-wasm/snappy" ]]; then
    cp -r "$HOME/git/snappy-wasm/snappy" "$SRC/snappy"
  else
    git clone --depth 1 https://github.com/google/snappy "$SRC/snappy"
  fi
fi
cmake_wasi "$SRC/snappy" "$SRC/snappy/build-wasi" \
  -DCMAKE_INSTALL_PREFIX="$DEPS/snappy" -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DSNAPPY_BUILD_TESTS=OFF -DSNAPPY_BUILD_BENCHMARKS=OFF
cmake --build "$SRC/snappy/build-wasi" --target install
echo "[deps] snappy -> $DEPS/snappy/lib/libsnappy.a" >&2

# --- liblzma (xz) ----------------------------------------------------------
# avro-c's lzma/xz codec. CMake's built-in FindLibLZMA picks it up via the
# LIBLZMA_LIBRARY/LIBLZMA_INCLUDE_DIR hints passed to the avro-c build. Threads
# off (wasi lacks pthread_sigmask); only the liblzma target (no xz CLI tools).
if [[ ! -d "$SRC/xz" ]]; then
  git clone --depth 1 https://github.com/tukaani-project/xz "$SRC/xz"
fi
rm -rf "$SRC/xz/build-wasi"
cmake_wasi "$SRC/xz" "$SRC/xz/build-wasi" \
  -DCMAKE_INSTALL_PREFIX="$DEPS/lzma" -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DXZ_THREADS=no -DXZ_NLS=OFF \
  -DXZ_TOOL_XZ=OFF -DXZ_TOOL_XZDEC=OFF -DXZ_TOOL_LZMADEC=OFF -DXZ_TOOL_LZMAINFO=OFF
cmake --build "$SRC/xz/build-wasi" --target liblzma
cmake --install "$SRC/xz/build-wasi"
echo "[deps] liblzma -> $DEPS/lzma/lib/liblzma.a" >&2

# --- avro-c ----------------------------------------------------------------
# DuckDB's FORK of avro-c (adds the Iceberg field-id API:
# avro_schema_record_field_id / avro_reader_reader / *_id), which the duckdb-avro
# extension requires -- stock apache/avro lacks these. Pinned to the duckdb vcpkg
# port's ref.
AVRO_REF="8af400279c445a81b8552a7670d8c1ebd92ba34a"
if [[ ! -d "$SRC/avro/.git" ]]; then
  rm -rf "$SRC/avro"
  git clone https://github.com/duckdb/duckdb-avro-c "$SRC/avro"
fi
git -C "$SRC/avro" checkout -q "$AVRO_REF"
AVROC="$SRC/avro/lang/c"
# wasi can't build SHARED libs; treat WASI like WIN32 for the shared target + install.
perl -0pi -e 's/if \(NOT WIN32\)\n# TODO: Create Windows DLLs/if (NOT WIN32 AND NOT CMAKE_SYSTEM_NAME STREQUAL "WASI")\n# TODO: Create Windows DLLs/' "$AVROC/src/CMakeLists.txt"
perl -0pi -e 's/install\(TARGETS avro-static avro-shared/install(TARGETS avro-static/' "$AVROC/src/CMakeLists.txt"
# Codecs: deflate (zlib) + snappy (libsnappy) + lzma/xz (liblzma) -- all three
# enabled via the libs we build for wasi. The fork's find_package(Snappy)/
# find_package(LibLZMA) are satisfied by the *_DIR / *_LIBRARY hints below.
#
# avro-c's lzma codec is non-interoperable as shipped: it names the codec "lzma"
# (the Avro spec / Java / fastavro use "xz") and uses *raw* LZMA2
# (lzma_raw_buffer_*) instead of the xz *container*. Patch it to (1) name + accept
# "xz", and (2) use the xz stream container (lzma_easy/stream_buffer_*) so it can
# read/write standard xz-compressed avro (e.g. Iceberg manifests).
perl -0pi -e 's/\tcodec->name = "lzma";/\tcodec->name = "xz";/' "$AVROC/src/codec.c"
perl -0pi -e 's/if \(strcmp\("lzma", type\) == 0\)/if (strcmp("lzma", type) == 0 || strcmp("xz", type) == 0)/' "$AVROC/src/codec.c"
perl -0pi -e 's/int64_t buff_len = len \+ lzma_raw_encoder_memusage\(filters\);/int64_t buff_len = lzma_stream_buffer_bound(len);/' "$AVROC/src/codec.c"
perl -0pi -e 's/ret = lzma_raw_buffer_encode\(filters, NULL, \(const uint8_t\*\)data, len, \(uint8_t\*\)codec->block_data, &written, codec->block_size\);/ret = lzma_easy_buffer_encode(LZMA_PRESET_DEFAULT, LZMA_CHECK_CRC64, NULL, (const uint8_t*)data, len, (uint8_t*)codec->block_data, \&written, codec->block_size);/' "$AVROC/src/codec.c"
perl -0pi -e 's/\tdo\n\t\{\n\t\tret = lzma_raw_buffer_decode\(filters, NULL, \(const uint8_t\*\)data,\n\t\t\t&read_pos, len, \(uint8_t\*\)codec->block_data, &write_pos,\n\t\t\tcodec->block_size\);/\tuint64_t memlimit = UINT64_MAX;\n\tdo\n\t{\n\t\tread_pos = 0; write_pos = 0;\n\t\tret = lzma_stream_buffer_decode(\&memlimit, 0, NULL, (const uint8_t*)data,\n\t\t\t\&read_pos, len, (uint8_t*)codec->block_data, \&write_pos,\n\t\t\tcodec->block_size);/' "$AVROC/src/codec.c"
rm -rf "$AVROC/build-wasi"
cmake_wasi "$AVROC" "$AVROC/build-wasi" \
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -Djansson_DIR="$DEPS/jansson/lib/cmake/jansson" -DSnappy_DIR="$DEPS/snappy/lib/cmake/Snappy" \
  -DCMAKE_PREFIX_PATH="$DEPS/jansson;$DEPS/snappy" \
  -DZLIB_LIBRARY="$ZLIB/lib/libz.a" -DZLIB_INCLUDE_DIR="$ZLIB/include" \
  -DLIBLZMA_LIBRARY="$DEPS/lzma/lib/liblzma.a" -DLIBLZMA_INCLUDE_DIR="$DEPS/lzma/include" \
  -DAVRO_BUILD_TESTS=OFF -DAVRO_BUILD_EXECUTABLES=OFF
cmake --build "$AVROC/build-wasi" --target avro-static
mkdir -p "$DEPS/avro-c/lib" "$DEPS/avro-c/include/avro"
cp "$(find "$AVROC/build-wasi" -name libavro.a | head -1)" "$DEPS/avro-c/lib/libavro.a"
cp "$AVROC/src/avro.h" "$DEPS/avro-c/include/"
cp "$AVROC/src/avro/"*.h "$DEPS/avro-c/include/avro/"
echo "[deps] avro-c -> $DEPS/avro-c/lib/libavro.a (deflate + snappy + lzma codecs)" >&2

# --- roaring (CRoaring) ----------------------------------------------------
if [[ ! -d "$SRC/roaring" ]]; then
  git clone --depth 1 https://github.com/RoaringBitmap/CRoaring "$SRC/roaring"
fi
cmake_wasi "$SRC/roaring" "$SRC/roaring/build-wasi" \
  -DCMAKE_INSTALL_PREFIX="$DEPS/roaring" \
  -DROARING_BUILD_STATIC=ON -DENABLE_ROARING_TESTS=OFF
cmake --build "$SRC/roaring/build-wasi" --target install
echo "[deps] roaring -> $DEPS/roaring/lib/libroaring.a" >&2

echo "[deps] done: jansson + avro-c + roaring built for wasm32-wasi under $DEPS" >&2
