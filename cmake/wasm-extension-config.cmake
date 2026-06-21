# DuckDB extensions to statically compile + embed into the wasm32-wasi static
# library (libduckdb-wasi.a).
#
# Passed to DuckDB's CMake via -DDUCKDB_EXTENSION_CONFIGS by
# scripts/build-libduckdb-wasm.sh. DuckDB's base extension/extension_config.cmake
# already loads `core_functions` and `parquet`; everything else here is OPT-IN.
#
# == Fully lean by default ==
# Each extension is embedded ONLY if named in EMBED_EXTENSIONS (a comma list,
# forwarded from the env by scripts/build-libduckdb-wasm.sh, or -DEMBED_EXTENSIONS=).
# With EMBED_EXTENSIONS empty the core embeds nothing extra (just DuckDB's base
# core_functions + parquet). The build script gates source staging/patching and
# dep-archive merging on the SAME list (ext_selected), so an unselected extension
# adds nothing to the archive. Build a fat core explicitly, e.g.:
#   EMBED_EXTENSIONS="httpfs,json,icu,spatial" ./scripts/build-libduckdb-wasm.sh
# Embedding is the per-extension counterpart of `duckdb-host compose --embed`
# (which embeds the Rust component extensions); both default to nothing.
#
# An extension also needs its prebuilt native deps present (the EXISTS guards
# below); selecting one whose deps aren't built simply skips it.

if(NOT DEFINED EMBED_EXTENSIONS AND DEFINED ENV{EMBED_EXTENSIONS})
  set(EMBED_EXTENSIONS "$ENV{EMBED_EXTENSIONS}")
endif()
string(REPLACE "," ";" _embed_list "${EMBED_EXTENSIONS}")
list(REMOVE_ITEM _embed_list "")
if(_embed_list)
  message(STATUS "wasm-extensions: EMBED_EXTENSIONS = ${_embed_list}")
else()
  message(STATUS "wasm-extensions: fully lean (no optional extensions embedded)")
endif()

# embed_ext(<name> [duckdb_extension_load args...]) -- load iff <name> is selected.
macro(embed_ext _name)
  list(FIND _embed_list "${_name}" _embed_idx)
  if(NOT _embed_idx EQUAL -1)
    duckdb_extension_load(${_name} ${ARGN})
    message(STATUS "wasm-extensions: embedding ${_name}")
  endif()
endmacro()

# want(<name>) -> sets WANT to TRUE/FALSE (for gating dep var-setup blocks).
macro(want _name)
  list(FIND _embed_list "${_name}" _want_idx)
  if(_want_idx EQUAL -1)
    set(WANT FALSE)
  else()
    set(WANT TRUE)
  endif()
endmacro()

# --- in-tree (no external deps) ---
embed_ext(json)
embed_ext(tpch)          # pure C++ data generator (dbgen)
embed_ext(tpcds)         # pure C++ data generator (dsdgen)
embed_ext(autocomplete)  # sql_auto_complete table function
embed_ext(icu)           # timezones + collations (TZ via getenv("TZ") + SET TimeZone; tzname stub in wasi-shim.hpp)

# --- out-of-tree (fetched via git; see docs/duckdb-official-extensions.md) ---
embed_ext(inet           # INET/IPv4/IPv6 type + functions (pure C++)
  GIT_URL https://github.com/duckdb/duckdb-inet
  GIT_TAG fe7f60bb60245197680fb07ecd1629a1dc3d91c8
)
embed_ext(fts            # full-text search (Porter stemmer, BM25; pure C++ + SQL macros)
  GIT_URL https://github.com/duckdb/duckdb-fts
  GIT_TAG 39376623630a968154bef4e6930d12ad0b59d7fb   # DuckDB-pinned commit for this version
  INCLUDE_DIR extension/fts/include                  # nested layout; so the generated loader finds fts_extension.hpp
)
embed_ext(vss            # vector similarity search (HNSW index; pure C++ usearch)
  GIT_URL https://github.com/duckdb/duckdb-vss
  GIT_TAG c8a4efe05003d8ef6eaad34f5521cf50126c9967   # DuckDB-pinned commit
  INCLUDE_DIR src/include
)
embed_ext(sqlite_scanner # read/attach SQLite database files (vendored sqlite3 + WASI VFS)
  GIT_URL https://github.com/duckdb/duckdb-sqlite
  GIT_TAG 833e105cbcaa0f6e8d34d334f3b920ce86f6fdf9   # DuckDB-pinned commit
  INCLUDE_DIR src/include
)
embed_ext(ducklake        # DuckLake lakehouse format (SQL catalog + parquet storage; pure C++, no native deps)
  GIT_URL https://github.com/duckdb/ducklake
  GIT_TAG 45788f0a875844ac8fed048c99b87f7f4b1c2ac1   # DuckDB-pinned commit
  INCLUDE_DIR src/include
)
embed_ext(encodings       # decode legacy text encodings (CSV in non-UTF8); pure C++, generated charset tables (~80 MB), no deps
  GIT_URL https://github.com/duckdb/duckdb-encodings
  GIT_TAG b5a547ec74fad87698ed3142033d7b9cf86e0b2f   # DuckDB 1.4.0-pinned commit (.github/config/extensions/encodings.cmake)
  INCLUDE_DIR src/include
)

# delta: read local Delta Lake tables (delta_scan). The in-tree extension wraps
# delta-kernel-rs; on wasm we use the kernel's SYNC engine (local std::fs only --
# no s3:// / object_store, which needs tokio+reqwest with no wasip2 transport).
# The kernel is prebuilt for wasm32-wasip2 by scripts/build-delta-kernel-wasm.sh
# (sync-engine, zstd+brotli codecs dropped to avoid C-symbol collisions) and
# staged + wired by scripts/build-libduckdb-wasm.sh (stage_delta_kernel). See
# cmake/delta-wasi/README.md.
if(EXISTS "${CMAKE_CURRENT_LIST_DIR}/../build/delta-kernel/out/libdelta_kernel_ffi.a")
  # SOURCE_DIR (not the bare in-tree form) so the include path defaults to
  # <src>/src/include -- the bare form hardcodes extension/delta/include, where
  # delta_extension.hpp does not live, breaking the generated loader.
  embed_ext(delta          # delta_scan('<local path>') over the sync engine
    SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}/../external/duckdb/extension/delta
  )
endif()

# aws: load_aws_credentials() + CREATE SECRET (TYPE s3, PROVIDER credential_chain).
# The AWS C++ SDK doesn't build for wasm, so the vendored extension resolves
# credentials natively (env vars + ~/.aws INI files + region) under __wasi__ --
# see external/duckdb/extension/aws/src/include/aws_wasi_credentials.hpp. The
# network/subprocess providers (sso/sts/instance/process) error clearly. Pairs
# with httpfs (which consumes the secret + signs S3 requests).
embed_ext(aws              # AWS credential resolution -> S3 secret for httpfs
  SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}/../external/duckdb/extension/aws
)

# azure: read Azure Blob Storage / Data Lake (az:// , abfss://). Wraps the Azure
# SDK for C++, which doesn't build under vcpkg for wasm -- so the SDK is prebuilt
# for wasm32-wasip2 (scripts/build-azure-sdk-wasm.sh, libcurl transport over
# curl-wasm, like httpfs) + merged into libduckdb-wasi.a. AzureCliCredential is
# unavailable (no subprocess); env / connection-string / SAS credentials work.
set(AZURE_SDK_WASM_DIR "${CMAKE_CURRENT_LIST_DIR}/../build/azure-sdk/out" CACHE PATH "" FORCE)
if(EXISTS "${AZURE_SDK_WASM_DIR}/lib/libazure-storage-blobs.a")
  embed_ext(azure          # az:// + abfss:// blob/datalake filesystem
    SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}/../external/duckdb/extension/azure
  )
endif()

# avro (read_avro) + iceberg. Both need C libs built for wasi by
# scripts/build-wasi-deps.sh into build/wasi-deps/: jansson + avro-c (deflate
# codec only -> no lzma/snappy) for the avro extension, and roaring (CRoaring)
# for iceberg. iceberg AutoLoadExtension("avro")s, so avro must be embedded too.
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
  embed_ext(avro          # read_avro table function (libavro-c + jansson, deflate codec)
    GIT_URL https://github.com/duckdb/duckdb-avro
    GIT_TAG 0c97a61781f63f8c5444cf3e0c6881ecbaa9fe13   # DuckDB-pinned commit
  )
  if(EXISTS "${WASI_DEPS}/roaring/lib/libroaring.a")
    set(roaring_DIR "${WASI_DEPS}/roaring/lib/cmake/roaring" CACHE PATH "" FORCE)
    embed_ext(iceberg     # Apache Iceberg tables (avro manifests + roaring; AWS SDK skipped on wasi)
      GIT_URL https://github.com/duckdb/duckdb-iceberg
      GIT_TAG 49d67e45a6f15ad855f3760658b4ab42967d9cdc # DuckDB-pinned commit
      INCLUDE_DIR src/include
    )
  endif()
endif()

# spatial: GEOS + PROJ + GDAL (+ tiff/jpeg/png/expat/sqlite/zlib) from the
# ~/git/*-wasm static libs (all wasi-sdk + libc++ compatible -- validated). The
# build script (scripts/build-libduckdb-wasm.sh) patches the fetched spatial
# CMakeLists to include() cmake/spatial-deps.cmake (which defines the GDAL::GDAL/
# PROJ::proj/GEOS::geos_c/EXPAT::EXPAT/sqlite3/ZLIB IMPORTED targets) in place of
# its find_package calls, extends the EMSCRIPTEN guards to WASI (network off), and
# merges the geo libs + cmake/spatial-deps/stubs.c into libduckdb-wasi.a.
if(EXISTS "$ENV{HOME}/git/gdal-wasm/build/deps/gdal/libgdal.a"
   AND EXISTS "$ENV{HOME}/git/geos-wasm/lib/lib/libgeos_c.a"
   AND EXISTS "$ENV{HOME}/git/proj-wasm/build_cbor/deps/proj/lib/libproj.a")
  embed_ext(spatial       # ST_* geometry (GEOS) + transforms (PROJ) + format I/O (GDAL/OGR)
    GIT_URL https://github.com/duckdb/duckdb-spatial
    GIT_TAG a6a607fe3a98ef9ad4bed218490b770f725fbc12   # DuckDB-pinned commit
    INCLUDE_DIR src/spatial                            # spatial_extension.hpp for the generated loader
  )
endif()

# excel: xlsx read/write (numformat + zip via minizip-ng + XML via expat). Needs
# minizip-ng built by scripts/build-wasi-deps.sh plus expat-wasm; cmake/excel-deps.cmake
# replaces its find_package(EXPAT/ZLIB/minizip-ng) and the libs merge into libduckdb-wasi.a.
if(EXISTS "${CMAKE_CURRENT_LIST_DIR}/../build/wasi-deps/minizip/lib/libminizip-ng.a"
   AND EXISTS "$ENV{HOME}/git/expat-wasm/build/lib/libexpat.a")
  embed_ext(excel          # read_xlsx/COPY TO xlsx + Excel number formatting
    GIT_URL https://github.com/duckdb/duckdb-excel
    GIT_TAG 8504be9ec8183e4082141f9359b53a64d3a440b7   # DuckDB-pinned commit
    INCLUDE_DIR src/excel/include                      # excel_extension.hpp for the generated loader
  )
endif()

# postgres_scanner: ATTACH/scan a PostgreSQL server over TCP (libpq). The pinned
# extension compiles libpq inline from a downloaded PG source; build-libduckdb-wasm.sh
# stages a wasi-cross-configured PG 15.13 tree (build-wasi-deps.sh) + injects the
# posix shim, replaces find_package(OpenSSL) with openssl-wasm, and adds a static
# build. Networking is httpfs's wasip2 socket graft (so embed httpfs too); TLS via
# openssl-wasm. Built static (no DONT_LINK) so it links into the core.
if(EXISTS "${CMAKE_CURRENT_LIST_DIR}/../build/wasi-deps/src/postgresql-15.13/src/include/pg_config.h"
   AND EXISTS "$ENV{HOME}/git/openssl-wasm/build/openssl/libssl.a")
  embed_ext(postgres_scanner # ATTACH/scan PostgreSQL over TCP (libpq compiled inline)
    GIT_URL https://github.com/duckdb/duckdb-postgres
    GIT_TAG f012a4f99cea1d276d1787d0dc84b1f1a0e0f0b2   # DuckDB-pinned commit
    INCLUDE_DIR src/include                            # postgres_scanner_extension.hpp for the loader
  )
endif()

# mysql_scanner: ATTACH/scan a MySQL/MariaDB server over TCP. Links a prebuilt
# MariaDB Connector/C (build-wasi-deps.sh, against openssl-wasm); cmake/mysql-deps.cmake
# replaces find_package(libmysql) + reuses the postgres socket graft + getaddrinfo
# wrapper + posix shim. Built static (the pin is DONT_LINK). Embed httpfs too.
if(EXISTS "${CMAKE_CURRENT_LIST_DIR}/../build/wasi-deps/mariadb/lib/mariadb/libmariadbclient.a"
   AND EXISTS "$ENV{HOME}/git/openssl-wasm/build/openssl/libssl.a")
  embed_ext(mysql_scanner    # ATTACH/scan MySQL/MariaDB over TCP (libmariadb)
    GIT_URL https://github.com/duckdb/duckdb-mysql
    GIT_TAG 8a32d4e069438585e80494e296e407653aebfed3   # DuckDB-pinned commit
    INCLUDE_DIR src/include                            # mysql_scanner_extension.hpp for the loader
  )
endif()

# httpfs / ui / uc_catalog all need CURL + OpenSSL from ~/git/curl-wasm +
# ~/git/openssl-wasm. The var-setup below only runs if at least one of them is
# selected (so a lean build doesn't re-enable the http module or wire curl).
#   - httpfs: HTTP/S3 filesystem over wasi:sockets (curl client; embedded CA
#     bundle via CURLOPT_CAINFO_BLOB). BSD sockets are grafted from the wasip2
#     libc into the wasip1 core module by scripts/build-libduckdb-wasm.sh.
#   - ui: the real DuckDB UI, bridged through the native host (duckdb-host ui).
#   - uc_catalog: ATTACH a Unity Catalog (raw libcurl + CA-bundle-blob patch).
want(httpfs)
set(_w_httpfs ${WANT})
want(ui)
set(_w_ui ${WANT})
want(uc_catalog)
set(_w_uc ${WANT})
set(OPENSSL_WASM_DIR "$ENV{HOME}/git/openssl-wasm/build/openssl")
set(CURL_WASM_DIR "$ENV{HOME}/git/curl-wasm/build")
if((_w_httpfs OR _w_ui OR _w_uc)
   AND EXISTS "${OPENSSL_WASM_DIR}/libcrypto.a"
   AND EXISTS "${CURL_WASM_DIR}/curl/lib/libcurl.a")
  if(_w_httpfs)
    # The toolchain sets DUCKDB_SKIP_HTTP (excludes src/main/http/http_util.cpp,
    # the HTTPUtil base classes httpfs links against). Re-enable for httpfs only.
    set(DUCKDB_SKIP_HTTP OFF CACHE BOOL "" FORCE)
  endif()
  # openssl-wasm has a flat layout (no lib/ subdir) -> set the result vars directly.
  # Headers split: generated (configuration.h/opensslv.h) under build/openssl/include,
  # source (macros.h + the rest) under third_party. Need both, build first.
  set(OPENSSL_FOUND TRUE CACHE BOOL "" FORCE)
  set(OPENSSL_INCLUDE_DIR "${OPENSSL_WASM_DIR}/include;$ENV{HOME}/git/openssl-wasm/third_party/openssl/include" CACHE STRING "" FORCE)
  set(OPENSSL_CRYPTO_LIBRARY "${OPENSSL_WASM_DIR}/libcrypto.a" CACHE FILEPATH "" FORCE)
  set(OPENSSL_SSL_LIBRARY "${OPENSSL_WASM_DIR}/libssl.a" CACHE FILEPATH "" FORCE)
  set(OPENSSL_LIBRARIES "${OPENSSL_WASM_DIR}/libssl.a;${OPENSSL_WASM_DIR}/libcrypto.a" CACHE STRING "" FORCE)
  set(OPENSSL_VERSION "3.6.2" CACHE STRING "" FORCE)
  set(CURL_ROOT "${CURL_WASM_DIR}/curl" CACHE PATH "" FORCE)
  set(CURL_INCLUDE_DIR "${CURL_WASM_DIR}/curl/include" CACHE PATH "" FORCE)
  set(CURL_LIBRARY "${CURL_WASM_DIR}/curl/lib/libcurl.a" CACHE FILEPATH "" FORCE)

  embed_ext(httpfs        # HTTP/S3 filesystem (httplib + openssl-wasm + curl-wasm + wasi:sockets)
    GIT_URL https://github.com/duckdb/duckdb-httpfs
    GIT_TAG 354d3f436a33f80f03a74419e76eb59459e19168   # DuckDB-pinned commit
    INCLUDE_DIR extension/httpfs/include
  )

  set(UI_WASM_STUB_DIR "${CMAKE_CURRENT_LIST_DIR}/ui-deps/wasm-stubs" CACHE PATH "" FORCE)
  if(EXISTS "${CMAKE_CURRENT_LIST_DIR}/../external/duckdb/extension/ui/src/http_server.cpp")
    embed_ext(ui            # DuckDB UI, bridged through the host (duckdb-host ui)
      SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}/../external/duckdb/extension/ui
    )
  endif()

  # uc_catalog v1.4.0 (@4638e9b): raw libcurl; find_package(CURL REQUIRED) is
  # satisfied by the vars above; apply_extension_patches swaps CURLOPT_CAINFO for
  # an embedded CURLOPT_CAINFO_BLOB and strips a stray catch.hpp include. Needs
  # httpfs + delta at runtime. Caveat: local-sync delta only (no remote s3:// data).
  embed_ext(uc_catalog      # ATTACH Unity Catalog (TYPE uc_catalog) -> delta tables
    GIT_URL https://github.com/duckdb/uc_catalog
    GIT_TAG 4638e9b86903ca3bfda03df9f396186e32c8d762   # "bump to v1.4.0" -- exact ABI match
    INCLUDE_DIR src/include
  )
endif()

# NOTE: DuckDB Community Extensions are NOT static-linked here. They inherit
# DuckDB's version-locked C++ ABI (the rebuild-per-release treadmill). The
# standard is to deliver their functionality as Rust *components* (the
# duckdb:extension WIT world) with the embed option -- version-independent and
# portable, like the in-repo extensions (isin, luhn, crypto, ...). See
# docs/duckdb-community-extensions.md for the feasibility map + the rationale.

# WASI VFS for sqlite_scanner's vendored sqlite3.c (-DSQLITE_OS_OTHER). Built
# unconditionally (tiny: vfs_wasi.c + os_init.c) and merged into libduckdb-wasi.a;
# only sqlite_scanner references it. Reused from ~/git/sqlite-wasm.
add_library(sqlite_wasivfs STATIC
  ${CMAKE_CURRENT_LIST_DIR}/sqlite-wasi-vfs/vfs_wasi.c
  ${CMAKE_CURRENT_LIST_DIR}/sqlite-wasi-vfs/os_init.c)
target_include_directories(sqlite_wasivfs PRIVATE ${CMAKE_CURRENT_LIST_DIR}/sqlite-wasi-vfs)
