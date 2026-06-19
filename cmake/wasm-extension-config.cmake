# DuckDB in-tree extensions to statically link into the wasm32-wasi static
# library (libduckdb-wasi.a).
#
# Passed to DuckDB's CMake via -DDUCKDB_EXTENSION_CONFIGS by
# scripts/build-libduckdb-wasm.sh. DuckDB's base extension/extension_config.cmake
# already loads `core_functions` and `parquet`, so we only add the extras here.
# Each entry makes DuckDB compile the extension's C++ into the archive AND list
# it in the generated builtin-extension loader (so it registers without a
# runtime LOAD). The matching `WASM_EXTENSIONS` env shorthand only toggles a
# flag; the real selection is here.
#
# Only DuckDB's *in-tree* extensions are eligible (those under
# external/duckdb/extension/): autocomplete, core_functions, icu, json, parquet,
# tpch, tpcds. Out-of-tree extensions (fts, httpfs, spatial, excel, inet, vss,
# sqlite_scanner, ...) live in separate repos and need register_external_extension
# + a feasibility pass on wasi (TLS/sockets/large deps) before they can be added.

# --- enabled ---
duckdb_extension_load(json)
duckdb_extension_load(tpch)          # pure C++ data generator (dbgen)
duckdb_extension_load(tpcds)         # pure C++ data generator (dsdgen)
duckdb_extension_load(autocomplete)  # sql_auto_complete table function
duckdb_extension_load(icu)           # timezones + collations (TZ via getenv("TZ") + SET TimeZone; tzname stub in wasi-shim.hpp)

# --- out-of-tree (fetched via git; see docs/duckdb-official-extensions.md) ---
duckdb_extension_load(inet           # INET/IPv4/IPv6 type + functions (pure C++)
  GIT_URL https://github.com/duckdb/duckdb-inet
  GIT_TAG fe7f60bb60245197680fb07ecd1629a1dc3d91c8
)
duckdb_extension_load(fts            # full-text search (Porter stemmer, BM25; pure C++ + SQL macros)
  GIT_URL https://github.com/duckdb/duckdb-fts
  GIT_TAG 39376623630a968154bef4e6930d12ad0b59d7fb   # DuckDB-pinned commit for this version
  INCLUDE_DIR extension/fts/include                  # nested layout; so the generated loader finds fts_extension.hpp
)
duckdb_extension_load(vss            # vector similarity search (HNSW index; pure C++ usearch)
  GIT_URL https://github.com/duckdb/duckdb-vss
  GIT_TAG c8a4efe05003d8ef6eaad34f5521cf50126c9967   # DuckDB-pinned commit
  INCLUDE_DIR src/include
)
duckdb_extension_load(sqlite_scanner # read/attach SQLite database files (vendored sqlite3 + WASI VFS)
  GIT_URL https://github.com/duckdb/duckdb-sqlite
  GIT_TAG 833e105cbcaa0f6e8d34d334f3b920ce86f6fdf9   # DuckDB-pinned commit
  INCLUDE_DIR src/include
)
duckdb_extension_load(ducklake        # DuckLake lakehouse format (SQL catalog + parquet storage; pure C++, no native deps)
  GIT_URL https://github.com/duckdb/ducklake
  GIT_TAG 45788f0a875844ac8fed048c99b87f7f4b1c2ac1   # DuckDB-pinned commit
  INCLUDE_DIR src/include
)

# avro (read_avro) + iceberg. Both need C libs built for wasi by
# scripts/build-wasi-deps.sh into build/wasi-deps/: jansson + avro-c (deflate
# codec only -> no lzma/snappy) for the avro extension, and roaring (CRoaring)
# for iceberg. iceberg AutoLoadExtension("avro")s, so avro must be present.
# scripts/build-libduckdb-wasm.sh patches duckdb-avro (drop lzma/snappy) +
# iceberg (skip AWS SDK/CURL on WASI like Emscripten) and merges the libs.
set(WASI_DEPS "${CMAKE_CURRENT_LIST_DIR}/../build/wasi-deps")
if(EXISTS "${WASI_DEPS}/avro-c/lib/libavro.a")
  # Pre-seed the libs duckdb-avro's find_library() looks for (avro/jansson/zlib);
  # lzma/snappy are patched out of its CMakeLists (deflate-only avro-c).
  set(AVRO_INCLUDE_DIR "${WASI_DEPS}/avro-c/include" CACHE PATH "" FORCE)
  set(AVRO_LIBRARY "${WASI_DEPS}/avro-c/lib/libavro.a" CACHE FILEPATH "" FORCE)
  set(JANSSON_LIBRARY "${WASI_DEPS}/jansson/lib/libjansson.a" CACHE FILEPATH "" FORCE)
  set(ZLIB_LIBRARY "$ENV{HOME}/git/curl-wasm/build/zlib/lib/libz.a" CACHE FILEPATH "" FORCE)
  duckdb_extension_load(avro          # read_avro table function (libavro-c + jansson, deflate codec)
    GIT_URL https://github.com/duckdb/duckdb-avro
    GIT_TAG 0c97a61781f63f8c5444cf3e0c6881ecbaa9fe13   # DuckDB-pinned commit
  )
  if(EXISTS "${WASI_DEPS}/roaring/lib/libroaring.a")
    set(roaring_DIR "${WASI_DEPS}/roaring/lib/cmake/roaring" CACHE PATH "" FORCE)
    duckdb_extension_load(iceberg     # Apache Iceberg tables (avro manifests + roaring; AWS SDK skipped on wasi)
      GIT_URL https://github.com/duckdb/duckdb-iceberg
      GIT_TAG 49d67e45a6f15ad855f3760658b4ab42967d9cdc # DuckDB-pinned commit
      INCLUDE_DIR src/include
    )
  endif()
endif()

# httpfs needs CURL (its curl client) + OpenSSL (crypto.cpp AES/EVP). Both come
# from ~/git/curl-wasm (libcurl 8.17 built for wasm + its own openssl/zlib/zstd),
# satisfying httpfs's find_package(CURL|OpenSSL). TLS for the httplib client is
# DuckDB's vendored mbedtls (already builds on wasi); networking is wasi:sockets.
# scripts/build-libduckdb-wasm.sh merges the curl-wasm libs into libduckdb-wasi.a.
# httpfs: HTTP/S3 filesystem over wasi:sockets. WORKING, OUT OF THE BOX --
# verified read_csv_auto('https://...') fetches over HTTPS + parses with secure
# cert verification, no settings needed. curl is the default client on wasi
# (scripts/build-libduckdb-wasm.sh patches httpfs LoadInternal).
#   - OpenSSL  -> ~/git/openssl-wasm (socket-capable: has BIO_new_socket, NOT
#     OPENSSL_NO_SOCK, real 3.6.2). TLS for both clients + crypto.cpp.
#   - CURL     -> ~/git/curl-wasm (libcurl + nghttp2/ngtcp2/nghttp3 + brotli +
#     zlib/zstd). curl's openssl symbols resolve from openssl-wasm (one openssl).
#   - httplib  -> DuckDB third_party/httplib; COMPILES on wasi
#     (AF_UNIX/AI_NUMERICHOST/NI_* patched) but FAILS AT RUNTIME (connect
#     select/poll gap), so curl is made the wasi default instead.
# BSD sockets: cargo-component builds a wasip1 core module (wasip1 libc has no
# sockets), so scripts/build-libduckdb-wasm.sh grafts the wasip2 libc socket +
# component-binding objects into libduckdb-wasi.a; they import wasi:sockets which
# the host grants (inherit_network). scripts/build-libduckdb-wasm.sh merges all libs.
# Cert verification works secure-by-default: scripts/build-libduckdb-wasm.sh
# embeds cmake/ca-bundle/cacert.pem and patches the curl client to load it via
# CURLOPT_CAINFO_BLOB (openssl-wasm can't load a CA file through the wrapped FS).
set(OPENSSL_WASM_DIR "$ENV{HOME}/git/openssl-wasm/build/openssl")
set(CURL_WASM_DIR "$ENV{HOME}/git/curl-wasm/build")
if(EXISTS "${OPENSSL_WASM_DIR}/libcrypto.a" AND EXISTS "${CURL_WASM_DIR}/curl/lib/libcurl.a")
  # The toolchain sets DUCKDB_SKIP_HTTP (excludes src/main/http/http_util.cpp,
  # the HTTPUtil/BaseRequest/HTTPHeaders base classes httpfs links against).
  # Re-enable the http module for httpfs (httplib is now wasi-patched).
  set(DUCKDB_SKIP_HTTP OFF CACHE BOOL "" FORCE)
  # openssl-wasm has a flat layout (no lib/ subdir) -> set the result vars directly.
  # openssl-wasm splits headers: generated (configuration.h/opensslv.h) under
  # build/openssl/include, source (macros.h + the rest) under third_party. Need
  # both, build first so the generated config wins.
  set(OPENSSL_FOUND TRUE CACHE BOOL "" FORCE)
  set(OPENSSL_INCLUDE_DIR "${OPENSSL_WASM_DIR}/include;$ENV{HOME}/git/openssl-wasm/third_party/openssl/include" CACHE STRING "" FORCE)
  set(OPENSSL_CRYPTO_LIBRARY "${OPENSSL_WASM_DIR}/libcrypto.a" CACHE FILEPATH "" FORCE)
  set(OPENSSL_SSL_LIBRARY "${OPENSSL_WASM_DIR}/libssl.a" CACHE FILEPATH "" FORCE)
  set(OPENSSL_LIBRARIES "${OPENSSL_WASM_DIR}/libssl.a;${OPENSSL_WASM_DIR}/libcrypto.a" CACHE STRING "" FORCE)
  set(OPENSSL_VERSION "3.6.2" CACHE STRING "" FORCE)
  set(CURL_ROOT "${CURL_WASM_DIR}/curl" CACHE PATH "" FORCE)
  set(CURL_INCLUDE_DIR "${CURL_WASM_DIR}/curl/include" CACHE PATH "" FORCE)
  set(CURL_LIBRARY "${CURL_WASM_DIR}/curl/lib/libcurl.a" CACHE FILEPATH "" FORCE)
  duckdb_extension_load(httpfs        # HTTP/S3 filesystem (httplib + openssl-wasm + curl-wasm + wasi:sockets)
    GIT_URL https://github.com/duckdb/duckdb-httpfs
    GIT_TAG 354d3f436a33f80f03a74419e76eb59459e19168   # DuckDB-pinned commit
    INCLUDE_DIR extension/httpfs/include
  )
endif()

# WASI VFS for sqlite_scanner's vendored sqlite3.c (built with -DSQLITE_OS_OTHER
# via the toolchain's SQLITE_WASI_FLAGS). vfs_wasi.c is reused from
# ~/git/sqlite-wasm; os_init.c provides sqlite3_os_init() registering it as the
# default VFS. scripts/build-libduckdb-wasm.sh builds this target and merges
# libsqlite_wasivfs.a into libduckdb-wasi.a so the core links it.
add_library(sqlite_wasivfs STATIC
  ${CMAKE_CURRENT_LIST_DIR}/sqlite-wasi-vfs/vfs_wasi.c
  ${CMAKE_CURRENT_LIST_DIR}/sqlite-wasi-vfs/os_init.c)
target_include_directories(sqlite_wasivfs PRIVATE ${CMAKE_CURRENT_LIST_DIR}/sqlite-wasi-vfs)

# --- deferred (out-of-tree) ---
# avro: CMakeLists find_path + vcpkg.json -> needs the Apache Avro C lib via
#   vcpkg built for wasi; no vcpkg toolchain here. @0c97a61.
# excel: CMakeLists find_package + vcpkg.json -> needs a vcpkg native dep
#   (xlsx/zip lib) built for wasi; no vcpkg toolchain here. @8504be9.
# spatial/httpfs/aws/azure/mysql_scanner/postgres_scanner/iceberg/ducklake/ui:
#   infeasible on wasi (sockets/TLS/huge native deps) -- see docs.
