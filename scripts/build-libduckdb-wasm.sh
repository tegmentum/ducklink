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

# DuckDB MUST be compiled for the same wasm target the component links against
# (wasm32-wasip2). The toolchain otherwise defaults to wasm32-wasip1-threads,
# which is a -pthread build where errno/TLS are thread-local; that thread-local
# access faults (out-of-bounds) in the single-threaded component runtime -- e.g.
# the SQL parser traps in process_integer_literal/core_yylex on the first parse.
# Keep this aligned with the component target.
export WASI_TARGET_TRIPLE=${WASI_TARGET_TRIPLE:-"wasm32-wasip2"}

WASM_EXTENSIONS=${WASM_EXTENSIONS:-"json"}

# Which in-tree DuckDB extensions get statically linked + registered as builtins
# is driven by this CMake config (duckdb_extension_load calls), NOT by
# WASM_EXTENSIONS (that env only flips DuckDB's WASM_ENABLED flag). Override with
# DUCKDB_EXTENSION_CONFIGS to point at a different file.
DUCKDB_EXTENSION_CONFIGS=${DUCKDB_EXTENSION_CONFIGS:-"$(pwd)/cmake/wasm-extension-config.cmake"}

BUILD_DIR=${BUILD_DIR:-"$(pwd)/build/duckdb-wasi"}
mkdir -p "$BUILD_DIR"

echo "Configuring DuckDB for wasm32-wasi in $BUILD_DIR" >&2
echo "  extension config: $DUCKDB_EXTENSION_CONFIGS" >&2
configure_duckdb() {
  # EXTRA_CXX_FLAGS / EXTRA_C_FLAGS append to the DuckDB compile (e.g.
  # EXTRA_CXX_FLAGS=-msimd128 to enable wasm SIMD autovectorization). Empty by
  # default; the runtime (wasmtime, browsers) supports SIMD.
  # Note: -msimd128 was measured as noise-level on sort/aggregation workloads
  # (the bottleneck was component COMPILE time, fixed by the host's on-disk
  # cache, not query compute), so it is intentionally not enabled by default.
  env WASM_EXTENSIONS="$WASM_EXTENSIONS" cmake -S "$DUCKDB_SOURCE_DIR" -B "$BUILD_DIR" \
    -DCMAKE_TOOLCHAIN_FILE="$(pwd)/cmake/toolchains/wasi-sdk.cmake" \
    -DWASI_SDK_PREFIX:PATH="$WASI_SDK_PREFIX" \
    -DDUCKDB_EXTENSION_CONFIGS="$DUCKDB_EXTENSION_CONFIGS" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_CXX_FLAGS="${EXTRA_CXX_FLAGS:-}" \
    -DCMAKE_C_FLAGS="${EXTRA_C_FLAGS:-${EXTRA_CXX_FLAGS:-}}" \
    -DBUILD_SHELL=OFF \
    -DBUILD_TESTS=OFF \
    -DBUILD_BENCHMARK=OFF \
    -DDUCKDB_PLATFORM="wasm32-wasi" \
    -DDUCKDB_LIBDYNAMIC=OFF \
    -DDUCKDB_LIBDUCKDB_STATIC=ON
}
# Patches for FetchContent-populated extension sources. Some are
# configure-blocking (avro's find_library(LZMA), iceberg's find_package(AWSSDK))
# and must be applied before configure can process that extension; since sources
# are fetched progressively, the loop below re-runs configure + patch until it
# succeeds. Every patch is idempotent and guards on its source being present.
apply_extension_patches() {
# Embed a CA bundle into httpfs's curl client so HTTPS certificate verification
# works without a host CA file: openssl's file BIO isn't reliably reachable
# through the component's wrapped filesystem, so we load the bundle from memory
# via CURLOPT_CAINFO_BLOB. Runs after configure (which FetchContent-populates the
# httpfs source) and before the build; idempotent (skips if already patched).
HTTPFS_SRC="$BUILD_DIR/_deps/httpfs_extension_fc-src/extension/httpfs"
CA_BUNDLE="$(pwd)/cmake/ca-bundle/cacert.pem"
if grep -q "duckdb_extension_load(httpfs" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -d "$HTTPFS_SRC" && -f "$CA_BUNDLE" ]]; then
  { printf '{'; xxd -i < "$CA_BUNDLE"; printf '}'; } > "$HTTPFS_SRC/duckdb_ca_bundle.inc"
  python3 - "$HTTPFS_SRC/httpfs_curl_client.cpp" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read()
if 'duckdb_wasi_ca_bundle' in s:
    sys.exit(0)
m = re.search(r'\n[ \t]*if \(!cert_path\.empty\(\)\) \{\n[ \t]*curl_easy_setopt\(curl, CURLOPT_CAINFO, cert_path\.c_str\(\)\);\n[ \t]*\}\n\}', s)
if not m:
    sys.stderr.write('CAINFO anchor not found in httpfs_curl_client.cpp\n'); sys.exit(1)
block = m.group(0)
inject = '''
#ifdef __wasi__
\t// wasi: openssl's file BIO can't reach the host filesystem reliably, so load
\t// an embedded CA bundle from memory (CURLOPT_CAINFO_BLOB, no file I/O).
\t{
\t\tstatic const unsigned char duckdb_wasi_ca_bundle[] =
#include "duckdb_ca_bundle.inc"
\t\t;
\t\tstruct curl_blob ca_blob;
\t\tca_blob.data = (void *)duckdb_wasi_ca_bundle;
\t\tca_blob.len = sizeof(duckdb_wasi_ca_bundle);
\t\tca_blob.flags = CURL_BLOB_COPY;
\t\tcurl_easy_setopt(curl, CURLOPT_CAINFO_BLOB, &ca_blob);
\t}
#endif
}'''
s = s.replace(block, block[:-1] + inject, 1)
open(p, 'w').write(s)
print('patched httpfs_curl_client.cpp for embedded CA bundle')
PY
  echo "Embedded CA bundle into httpfs curl client ($(grep -c 'BEGIN CERTIFICATE' "$CA_BUNDLE") certs)" >&2

  # Make curl the default HTTP client on wasi: the vendored httplib client
  # compiles but its non-blocking connect (select/poll) doesn't work on wasi, so
  # `read_csv('https://...')` must use curl. Remap the default -> curl and seed
  # config.http_util with HTTPFSCurlUtil. Idempotent (marker: wasi-default-curl).
  python3 - "$HTTPFS_SRC/httpfs_extension.cpp" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
if 'wasi-default-curl' in s:
    sys.exit(0)
# 1) initial default assignment in LoadInternal
init_old = ('\t} else {\n'
            '\t\tconfig.http_util = make_shared_ptr<HTTPFSUtil>();\n'
            '\t}')
init_new = ('\t} else {\n'
            '#ifdef __wasi__\n'
            '\t\tconfig.http_util = make_shared_ptr<HTTPFSCurlUtil>(); // wasi-default-curl\n'
            '#else\n'
            '\t\tconfig.http_util = make_shared_ptr<HTTPFSUtil>();\n'
            '#endif\n'
            '\t}')
# 2) inside the SET callback, remap "default" -> "curl" on wasi
cb_old = '#ifndef EMSCRIPTEN\n\t\tif (value == "curl") {'
cb_new = ('#ifndef EMSCRIPTEN\n'
          '#ifdef __wasi__\n'
          '\t\tif (value == "default") {\n'
          '\t\t\tvalue = "curl";\n'
          '\t\t}\n'
          '#endif\n'
          '\t\tif (value == "curl") {')
for old, new, what in ((init_old, init_new, 'LoadInternal default'),
                       (cb_old, cb_new, 'SET callback')):
    if old not in s:
        sys.stderr.write('anchor not found: %s\n' % what); sys.exit(1)
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched httpfs_extension.cpp: curl is the default client on wasi')
PY
fi

# duckdb-avro: our wasi avro-c is deflate-only (no lzma/snappy), so drop those
# REQUIRED find_library() calls + their use in ALL_AVRO_LIBRARIES. Idempotent.
AVRO_SRC="$BUILD_DIR/_deps/avro_extension_fc-src"
if grep -q "duckdb_extension_load(avro" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -f "$AVRO_SRC/CMakeLists.txt" ]]; then
  python3 - "$AVRO_SRC/CMakeLists.txt" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
if 'wasi-no-lzma-snappy' in s:
    sys.exit(0)
# drop the lzma/snappy REQUIRED finds (both MSVC and non-MSVC spellings)
for pat in [r'\n\s*find_library\(LZMA_LIBRARY [^\)]*REQUIRED\)',
            r'\n\s*find_library\(SNAPPY_LIBRARY [^\)]*REQUIRED\)']:
    s = re.sub(pat, '', s)
# drop their use in ALL_AVRO_LIBRARIES (+ jemalloc/gmp/math which we don't provide)
for var in ('LZMA_LIBRARY', 'SNAPPY_LIBRARY', 'JEMALLOC_LIBRARY', 'GMP_LIBRARY', 'MATH_LIBRARY'):
    s = re.sub(r'\n\s*\$\{%s\}' % var, '', s)
s = '# wasi-no-lzma-snappy\n' + s
open(p, 'w').write(s)
print('patched duckdb-avro CMakeLists: deflate-only (no lzma/snappy/jemalloc/gmp)')
PY
fi

# iceberg: upstream finds the AWS C++ SDK + CURL behind `NOT Emscripten` guards.
# On WASI we (1) skip those in CMake, (2) skip the AWS-SDK includes/decls in
# aws.hpp, and (3) replace aws.cpp's AWS-SDK request path with a self-contained
# SigV4 signer (cmake/iceberg-wasi/aws_wasi.inc) that issues signed requests via
# HTTPUtil (curl) -- so AWS-native Iceberg catalogs (Glue/S3 Tables) work without
# the AWS SDK. EMSCRIPTEN keeps the upstream stub.
ICE_SRC="$BUILD_DIR/_deps/iceberg_extension_fc-src"
AWS_WASI_INC="$(pwd)/cmake/iceberg-wasi/aws_wasi.inc"
if grep -q "duckdb_extension_load(iceberg" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -d "$ICE_SRC" ]]; then
  # CMake: skip AWS/CURL find_package on WASI as well as Emscripten
  if ! grep -q 'STREQUAL "WASI"' "$ICE_SRC/CMakeLists.txt"; then
    perl -0pi -e 's/NOT CMAKE_SYSTEM_NAME STREQUAL "Emscripten"/NOT CMAKE_SYSTEM_NAME STREQUAL "Emscripten" AND NOT CMAKE_SYSTEM_NAME STREQUAL "WASI"/g' "$ICE_SRC/CMakeLists.txt"
  fi
  # aws.hpp: skip the AWS-SDK includes + AWS-typed method decls on wasi too.
  [[ -f "$ICE_SRC/src/include/aws.hpp" ]] && \
    perl -0pi -e 's/#ifdef EMSCRIPTEN/#if defined(EMSCRIPTEN) || defined(__wasi__)/g' "$ICE_SRC/src/include/aws.hpp"
  # aws.cpp: inject the real SigV4 impl for __wasi__ (keep EMSCRIPTEN stub + AWS SDK).
  if [[ -f "$ICE_SRC/src/aws.cpp" ]]; then
    python3 - "$ICE_SRC/src/aws.cpp" "$AWS_WASI_INC" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'aws_wasi.inc' in s:
    sys.exit(0)
# (1) includes guard: wasi pulls <time.h>/<algorithm>, not the AWS SDK headers
inc_old = '#ifdef EMSCRIPTEN\n#else\n#include <aws/core/auth/AWSCredentialsProviderChain.h>'
inc_new = ('#if defined(__wasi__)\n#include <time.h>\n#include <algorithm>\n'
           '#include "duckdb/main/database.hpp"\n'
           '#elif defined(EMSCRIPTEN)\n#else\n#include <aws/core/auth/AWSCredentialsProviderChain.h>')
# (2) impl guard: wasi includes the SigV4 impl; EMSCRIPTEN keeps the GET stub
impl_old = ('#ifdef EMSCRIPTEN\n\n'
            'unique_ptr<HTTPResponse> AWSInput::GetRequest(ClientContext &context) {\n'
            '\tthrow NotImplementedException("GET on WASM not implemented yet");\n}\n\n#else')
impl_new = ('#if defined(__wasi__)\n#include "%s"\n#elif defined(EMSCRIPTEN)\n\n'
            'unique_ptr<HTTPResponse> AWSInput::GetRequest(ClientContext &context) {\n'
            '\tthrow NotImplementedException("GET on WASM not implemented yet");\n}\n\n#else') % inc
for old, new, what in ((inc_old, inc_new, 'aws.cpp includes guard'),
                       (impl_old, impl_new, 'aws.cpp impl guard')):
    if old not in s:
        sys.stderr.write('anchor not found: %s\n' % what); sys.exit(1)
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched iceberg aws.cpp: wasi SigV4 signer injected')
PY
  fi
  echo "patched iceberg: AWS SDK/CURL skipped on wasi; SigV4 signer wired" >&2
fi

# spatial: replace its find_package(GDAL/PROJ/EXPAT/sqlite/ZLIB/GEOS) with our
# IMPORTED targets (cmake/spatial-deps.cmake, backed by the ~/git/*-wasm libs)
# and turn network off on wasi (like the upstream Emscripten path).
SPATIAL_SRC="$BUILD_DIR/_deps/spatial_extension_fc-src"
SPATIAL_DEPS_CMAKE="$(pwd)/cmake/spatial-deps.cmake"
if grep -q "duckdb_extension_load(spatial" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -f "$SPATIAL_SRC/CMakeLists.txt" ]]; then
  python3 - "$SPATIAL_SRC/CMakeLists.txt" "$SPATIAL_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'spatial-deps.cmake' in s:
    sys.exit(0)
# 1) replace the find_package block with include(our imported targets)
fp_old = ('find_package(ZLIB REQUIRED)\n'
          'find_package(PROJ CONFIG REQUIRED)\n'
          'find_package(GDAL CONFIG REQUIRED)\n'
          'find_package(EXPAT REQUIRED)\n'
          'find_package(unofficial-sqlite3 CONFIG REQUIRED)')
fp_new = 'include("%s")' % inc
# 2) GEOS is found separately; our include() already defines GEOS::geos_c
geos_old = '  find_package(GEOS REQUIRED)\n'
# 3) network off on wasi too (matches the Emscripten branch)
net_old = ('if(EMSCRIPTEN)\n'
           '  message(STATUS "Building for Emscripten, disabling network functionality")\n'
           '  set(SPATIAL_USE_NETWORK OFF)\n'
           'endif()')
net_new = ('if(EMSCRIPTEN OR CMAKE_SYSTEM_NAME STREQUAL "WASI")\n'
           '  message(STATUS "Disabling network functionality")\n'
           '  set(SPATIAL_USE_NETWORK OFF)\n'
           'endif()')
for old, new, what in ((fp_old, fp_new, 'find_package block'),
                       (geos_old, '', 'GEOS find_package'),
                       (net_old, net_new, 'network guard')):
    if old not in s:
        sys.stderr.write('spatial anchor not found: %s\n' % what); sys.exit(1)
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched spatial CMakeLists: IMPORTED geo deps + network off on wasi')
PY
  # proj_db.c: the extension embeds an OLDER proj.db (DATABASE.LAYOUT.VERSION
  # MINOR=2) than proj-wasm's libproj (PROJ 9.x rejects layout < 1.6).
  # Regenerate it (xxd -i -> proj_db[]/proj_db_len) from the matching proj.db.
  PROJ_DB="$HOME/git/proj-wasm/build_real_sqlite/deps/proj/data/proj.db"
  PROJ_DB_C="$SPATIAL_SRC/src/spatial/modules/proj/proj_db.c"
  if [[ -f "$PROJ_DB" && -f "$PROJ_DB_C" ]]; then
    _sz="$(wc -c < "$PROJ_DB" | tr -d ' ')"
    if ! grep -q "proj_db_len = $_sz" "$PROJ_DB_C" 2>/dev/null; then
      _t="$(mktemp -d)"; cp "$PROJ_DB" "$_t/proj.db"
      ( cd "$_t" && xxd -i proj.db ) > "$PROJ_DB_C"
      rm -rf "$_t"
      echo "regenerated spatial proj_db.c from proj-wasm proj.db (layout 1.6)" >&2
    fi
  fi
fi

# excel: replace its find_package(EXPAT/ZLIB/minizip-ng) with our IMPORTED
# targets (cmake/excel-deps.cmake, backed by expat-wasm + curl-wasm zlib +
# the minizip-ng built by build-wasi-deps.sh).
EXCEL_SRC="$BUILD_DIR/_deps/excel_extension_fc-src"
EXCEL_DEPS_CMAKE="$(pwd)/cmake/excel-deps.cmake"
if grep -q "duckdb_extension_load(excel" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -f "$EXCEL_SRC/CMakeLists.txt" ]]; then
  python3 - "$EXCEL_SRC/CMakeLists.txt" "$EXCEL_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'excel-deps.cmake' in s:
    sys.exit(0)
fp_old = ('find_package(EXPAT REQUIRED)\n'
          'find_package(ZLIB REQUIRED)\n'
          'find_package(minizip-ng CONFIG REQUIRED)')
if fp_old not in s:
    sys.stderr.write('excel anchor not found: find_package block\n'); sys.exit(1)
s = s.replace(fp_old, 'include("%s")' % inc, 1)
open(p, 'w').write(s)
print('patched excel CMakeLists: IMPORTED EXPAT + ZLIB + minizip-ng deps')
PY
fi

# postgres_scanner: the pinned extension (f012a4f) compiles libpq's sources
# inline from a downloaded PostgreSQL tree and is DONT_LINK (loadable only).
# For the static wasm core we: replace find_package(OpenSSL) with our deps
# (cmake/postgres-deps.cmake -> openssl-wasm + shim force-include); add a
# build_static_extension call; drop the getaddrinfo/gettimeofday fallback files
# (wasi has those); and stage the wasi-cross-configured PG 15.13 source (built
# by build-wasi-deps.sh) as the extension's `postgres/` tree so it skips its own
# download + host ./configure. Networking comes from httpfs's wasip2 graft.
PG_SRC="$BUILD_DIR/_deps/postgres_scanner_extension_fc-src"
PG_DEPS_CMAKE="$(pwd)/cmake/postgres-deps.cmake"
PG_STAGED="$(pwd)/build/wasi-deps/src/postgresql-15.13"
if grep -q "duckdb_extension_load(postgres_scanner" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -f "$PG_SRC/CMakeLists.txt" ]]; then
  python3 - "$PG_SRC/CMakeLists.txt" "$PG_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'postgres-deps.cmake' in s:
    sys.exit(0)
if 'find_package(OpenSSL REQUIRED)' not in s:
    sys.stderr.write('postgres anchor not found: find_package(OpenSSL)\n'); sys.exit(1)
s = s.replace('find_package(OpenSSL REQUIRED)', 'include("%s")' % inc, 1)
# wasi provides getaddrinfo/getnameinfo/gettimeofday; drop pg's fallback files.
# fe-print.c (libpq's PQprint result pretty-printer, unused by the scanner)
# includes <sys/ioctl.h> which conflicts with the duckdb toolchain ioctl stub.
for f in ('postgres/src/port/getaddrinfo.c', 'postgres/src/port/gettimeofday.c',
          'postgres/src/interfaces/libpq/fe-print.c'):
    s = s.replace('    %s\n' % f, '')
# the extension is DONT_LINK upstream (loadable only); add a static build so it
# links into the wasm core, with the same sources.
anchor = ('build_loadable_extension(${TARGET_NAME} ${PARAMETERS} ${ALL_OBJECT_FILES}\n'
          '                         ${LIBPG_SOURCES_FULLPATH})')
if anchor not in s:
    sys.stderr.write('postgres anchor not found: build_loadable_extension\n'); sys.exit(1)
# f012a4f is DONT_LINK (loadable only) -> no install(EXPORT) for the static
# target. build_static_extension creates ${TARGET_NAME}_extension; export it
# like the in-tree extensions do, else "not in any export set".
static = ('build_static_extension(${TARGET_NAME} ${ALL_OBJECT_FILES}\n'
          '                       ${LIBPG_SOURCES_FULLPATH})\n'
          'install(TARGETS ${TARGET_NAME}_extension EXPORT "${DUCKDB_EXPORT_SET}"\n'
          '        LIBRARY DESTINATION "${INSTALL_LIB_DIR}"\n'
          '        ARCHIVE DESTINATION "${INSTALL_LIB_DIR}")')
s = s.replace(anchor, anchor + '\n' + static, 1)
open(p, 'w').write(s)
print('patched postgres CMakeLists: openssl deps + static build + export + drop pg fallbacks')
PY
  # stage the wasi-configured PG 15.13 source as the extension's postgres/ tree,
  # force-replacing the host source the extension's own ./configure downloads
  # (so the wasi pg_config.h is used, not a host one). -L: replace if not ours.
  if [[ -d "$PG_STAGED/src/include" && -f "$PG_STAGED/src/include/pg_config.h" ]]; then
    if [[ ! -L "$PG_SRC/postgres" ]]; then
      rm -rf "$PG_SRC/postgres"
      ln -s "$PG_STAGED" "$PG_SRC/postgres"
      echo "staged wasi PG 15.13 source as postgres_scanner/postgres" >&2
    fi
  else
    echo "WARNING: PG 15.13 source not configured at $PG_STAGED (run build-wasi-deps.sh)" >&2
  fi
fi

# mysql_scanner: like postgres but links a PREBUILT MariaDB Connector/C
# (libmariadbclient.a). Replace find_package(libmysql) with cmake/mysql-deps.cmake
# (openssl-wasm + the shim + PG_WASI_REAL_NETDB) and add a static build (the
# pinned extension is DONT_LINK / loadable only). Networking reuses the postgres
# socket graft + getaddrinfo wrapper.
MY_SRC="$BUILD_DIR/_deps/mysql_scanner_extension_fc-src"
MY_DEPS_CMAKE="$(pwd)/cmake/mysql-deps.cmake"
if grep -q "duckdb_extension_load(mysql_scanner" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
   && [[ -f "$MY_SRC/CMakeLists.txt" ]]; then
  python3 - "$MY_SRC/CMakeLists.txt" "$MY_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'mysql-deps.cmake' in s:
    sys.exit(0)
if 'find_package(libmysql REQUIRED)' not in s:
    sys.stderr.write('mysql anchor not found: find_package(libmysql)\n'); sys.exit(1)
s = s.replace('find_package(libmysql REQUIRED)', 'include("%s")' % inc, 1)
anchor = 'build_loadable_extension(${TARGET_NAME} ${PARAMETERS} ${ALL_OBJECT_FILES})'
if anchor not in s:
    sys.stderr.write('mysql anchor not found: build_loadable_extension\n'); sys.exit(1)
add = (anchor + '\n'
       'build_static_extension(${TARGET_NAME} ${ALL_OBJECT_FILES})\n'
       'target_include_directories(${TARGET_NAME}_extension PRIVATE include ${MYSQL_INCLUDE_DIR})\n'
       'install(TARGETS ${TARGET_NAME}_extension EXPORT "${DUCKDB_EXPORT_SET}"\n'
       '        LIBRARY DESTINATION "${INSTALL_LIB_DIR}"\n'
       '        ARCHIVE DESTINATION "${INSTALL_LIB_DIR}")')
s = s.replace(anchor, add, 1)
open(p, 'w').write(s)
print('patched mysql CMakeLists: libmariadb deps + static build + export')
PY
  # postgres + mysql both vendor these database-connector helpers in the duckdb
  # namespace with different bodies (e.g. EscapeConnectionString escapes ' vs ")
  # -> duplicate-symbol clash when both are linked. They're single-file in the
  # mysql extension, so give them internal linkage (static).
  python3 - "$MY_SRC/src/storage/mysql_catalog.cpp" "$MY_SRC/src/storage/mysql_schema_entry.cpp" <<'PY'
import sys
defs = {
  'mysql_catalog.cpp': [('string EscapeConnectionString(const string &input) {',
                         'static string EscapeConnectionString(const string &input) {'),
                        ('unique_ptr<SecretEntry> GetSecret(ClientContext &context, const string &secret_name) {',
                         'static unique_ptr<SecretEntry> GetSecret(ClientContext &context, const string &secret_name) {')],
  'mysql_schema_entry.cpp': [('bool CatalogTypeIsSupported(CatalogType type) {',
                              'static bool CatalogTypeIsSupported(CatalogType type) {')],
}
for path in sys.argv[1:]:
    name = path.rsplit('/', 1)[-1]
    s = open(path).read()
    for old, new in defs.get(name, []):
        if old in s and new not in s:
            s = s.replace(old, new, 1)
    open(path, 'w').write(s)
print('patched mysql: static linkage for shared duckdb-namespace helpers')
PY
  # MariaDB Connector/C lacks MYSQL_OPT_SSL_MODE (the extension's mechanism); map
  # ssl_mode to MariaDB's MYSQL_OPT_SSL_ENFORCE on wasi.
  python3 - "$MY_SRC/src/mysql_utils.cpp" <<'PY'
import sys
p=sys.argv[1]; s=open(p).read()
old='''	if (config.ssl_mode != SSL_MODE_PREFERRED) {
		mysql_options(mysql, MYSQL_OPT_SSL_MODE, &config.ssl_mode);
	}'''
new='''#ifdef __wasi__
	{
		my_bool _ssl_enforce = (config.ssl_mode == SSL_MODE_REQUIRED ||
		                        config.ssl_mode == SSL_MODE_VERIFY_CA ||
		                        config.ssl_mode == SSL_MODE_VERIFY_IDENTITY) ? 1 : 0;
		mysql_options(mysql, MYSQL_OPT_SSL_ENFORCE, &_ssl_enforce);
	}
#else
	if (config.ssl_mode != SSL_MODE_PREFERRED) {
		mysql_options(mysql, MYSQL_OPT_SSL_MODE, &config.ssl_mode);
	}
#endif'''
if old in s and '_ssl_enforce' not in s:
    open(p,'w').write(s.replace(old,new,1)); print('patched mysql ssl_mode -> SSL_ENFORCE')
PY
fi
}

# delta: the `delta` extension (duckdb-delta @ fa847248, vendored at
# external/duckdb/extension/delta, matching this DuckDB) wraps delta-kernel-rs.
# On wasm it uses the kernel's SYNC engine only (local std::fs; no tokio/reqwest/
# object_store). The kernel is prebuilt for wasm32-wasip2 (sync engine, zstd/brotli
# codecs dropped) by scripts/build-delta-kernel-wasm.sh; the vendored extension
# source + CMakeLists are already patched for wasm (get_sync_engine, DEFINE_SYNC_ENGINE,
# no-op kernel ExternalProject) -- see cmake/delta-wasi/. This just stages the
# prebuilt .a where the patched CMakeLists' DELTA_KERNEL_LIBPATH expects it.
# Runs before configure because the delta CMakeLists is read at configure time.
stage_delta_kernel() {
  grep -q "duckdb_extension_load(delta" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null || return 0
  local OUT_DIR="$(pwd)/build/delta-kernel/out"
  if [[ ! -f "$OUT_DIR/libdelta_kernel_ffi.a" ]]; then
    echo "delta: building wasm sync-engine kernel" >&2
    OUT_DIR="$OUT_DIR" WASI_SDK_PREFIX="$WASI_SDK_PREFIX" \
      "$(pwd)/scripts/build-delta-kernel-wasm.sh"
  fi
  # Apply the wasm C++ patches to the vendored duckdb-delta (fa847248) source
  # (idempotent; guarded on the engine-swap marker). Lets a freshly re-vendored
  # pristine fa847248 be made wasm-ready without manual edits.
  local DELTA_DIR="$DUCKDB_SOURCE_DIR/extension/delta"
  if [[ -f "$DELTA_DIR/src/functions/delta_scan/delta_multi_file_list.cpp" ]] \
     && ! grep -q 'get_sync_engine' "$DELTA_DIR/src/functions/delta_scan/delta_multi_file_list.cpp"; then
    for p in "$(pwd)"/cmake/delta-wasi/delta-fa847248-*.patch; do
      ( cd "$DELTA_DIR" && git apply "$p" 2>/dev/null ) \
        || patch -p1 -d "$DELTA_DIR" < "$p"
    done
    echo "delta: applied fa847248 wasm C++ patches" >&2
  fi

  # Pre-place the kernel .a (release; copied to debug too for the byproduct decl).
  local KDIR="$BUILD_DIR/rust/src/delta_kernel/target"
  mkdir -p "$KDIR/wasm32-wasip2/release" "$KDIR/wasm32-wasip2/debug" "$KDIR/ffi-headers"
  cp "$OUT_DIR/libdelta_kernel_ffi.a" "$KDIR/wasm32-wasip2/release/"
  cp "$OUT_DIR/libdelta_kernel_ffi.a" "$KDIR/wasm32-wasip2/debug/"
  cp "$OUT_DIR/ffi-headers/"* "$KDIR/ffi-headers/" 2>/dev/null || true
  echo "delta: staged prebuilt wasm kernel ($(du -h "$OUT_DIR/libdelta_kernel_ffi.a" | cut -f1))" >&2
}

# aws: the `aws` extension resolves AWS credentials for httpfs's S3 secrets. The
# AWS C++ SDK doesn't build for wasm, so we vendor duckdb-aws (the version-matched
# pin) + patch it to resolve credentials natively (env vars + ~/.aws INI files +
# region) under __wasi__ via cmake/aws-wasi/aws_wasi_credentials.hpp. Pure C++,
# no extra deps. Runs before configure (in-tree extension read at configure time).
stage_aws_extension() {
  grep -q "duckdb_extension_load(aws" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null || return 0
  local AWS_DIR="$DUCKDB_SOURCE_DIR/extension/aws"
  local PIN="812ce80fde0bfa6e4641b6fd798087349a610795"
  if [[ ! -f "$AWS_DIR/CMakeLists.txt" ]]; then
    echo "aws: vendoring duckdb-aws @ $PIN" >&2
    local tmp="$BUILD_DIR/duckdb-aws-src"
    if [[ ! -d "$tmp/.git" ]]; then
      git clone --quiet https://github.com/duckdb/duckdb-aws "$tmp"
      git -C "$tmp" checkout --quiet "$PIN"
    fi
    mkdir -p "$AWS_DIR"
    ( cd "$tmp" && git archive "$PIN" CMakeLists.txt src ) | tar -x -C "$AWS_DIR"
  fi
  # Apply the wasm patches + drop in the native resolver header (idempotent).
  if ! grep -q '__wasi__' "$AWS_DIR/src/aws_secret.cpp" 2>/dev/null; then
    cp "$(pwd)/cmake/aws-wasi/aws_wasi_credentials.hpp" "$AWS_DIR/src/include/"
    for p in "$(pwd)"/cmake/aws-wasi/aws-812ce80-*.patch; do
      ( cd "$AWS_DIR" && git apply "$p" 2>/dev/null ) || patch -p1 -d "$AWS_DIR" < "$p"
    done
    echo "aws: applied wasm patches + native credential resolver" >&2
  fi
}

# azure: build the Azure SDK for wasm (if needed) + vendor/patch the extension.
# Wraps the Azure SDK for C++; the SDK is prebuilt for wasm32-wasip2 (libcurl
# transport over curl-wasm) and merged into libduckdb-wasi.a below.
stage_azure_extension() {
  grep -q "duckdb_extension_load(azure" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null || return 0
  if [[ ! -f "$(pwd)/build/azure-sdk/out/lib/libazure-storage-blobs.a" ]]; then
    echo "azure: building Azure SDK for wasm" >&2
    WASI_SDK_PREFIX="$WASI_SDK_PREFIX" "$(pwd)/scripts/build-azure-sdk-wasm.sh"
  fi
  local AZ_DIR="$DUCKDB_SOURCE_DIR/extension/azure"
  local PIN="5e458fcc466d2bc421922b11f4316564e3017800"
  if [[ ! -f "$AZ_DIR/CMakeLists.txt" ]]; then
    echo "azure: vendoring duckdb-azure @ $PIN" >&2
    local tmp="$BUILD_DIR/duckdb-azure-src"
    if [[ ! -d "$tmp/.git" ]]; then
      git clone --quiet https://github.com/duckdb/duckdb-azure "$tmp"
      git -C "$tmp" checkout --quiet "$PIN"
    fi
    mkdir -p "$AZ_DIR"
    ( cd "$tmp" && git archive "$PIN" CMakeLists.txt src ) | tar -x -C "$AZ_DIR"
  fi
  # Patch the CMakeLists (prebuilt SDK headers + build_static_extension) + the
  # curl transport (embedded CA bundle via CURLOPT_CAINFO_BLOB) for wasm. Idempotent.
  if ! grep -q 'AZURE_SDK_WASM_DIR' "$AZ_DIR/CMakeLists.txt" 2>/dev/null; then
    for p in "$(pwd)"/cmake/azure-deps/azure-5e458fcc-CMakeLists.txt.patch \
             "$(pwd)"/cmake/azure-deps/azure-5e458fcc-ca-bundle-blob.patch; do
      ( cd "$AZ_DIR" && git apply "$p" 2>/dev/null ) || patch -p1 -d "$AZ_DIR" < "$p"
    done
    echo "azure: applied wasm CMakeLists + CA-bundle patches" >&2
  fi
  # Embed the CA bundle (httpfs's) for CURLOPT_CAINFO_BLOB -- openssl-wasm can't
  # load a CA file through the wrapped FS. Regenerate if missing/stale.
  local CA_BUNDLE="$(pwd)/cmake/ca-bundle/cacert.pem"
  if [[ -f "$CA_BUNDLE" && ! -f "$AZ_DIR/src/azure_ca_bundle.inc" ]]; then
    { printf '{'; xxd -i < "$CA_BUNDLE"; printf '}'; } > "$AZ_DIR/src/azure_ca_bundle.inc"
    echo "azure: embedded CA bundle ($(grep -c 'BEGIN CERTIFICATE' "$CA_BUNDLE") certs)" >&2
  fi
}

# ui: the real DuckDB UI. httplib can't listen() in the wasip2 sandbox, so the
# native host (duckdb-host ui) owns the socket and bridges each request to the
# extension's HttpServer::HandleRequest (exposed as duckdb_ui_handle_request).
# Vendor duckdb-ui @ ded075b (DuckDB 1.4.0) + apply the wasm patches (cmake/ui-deps/).
stage_ui_extension() {
  grep -q "duckdb_extension_load(ui" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null || return 0
  local UI_DIR="$DUCKDB_SOURCE_DIR/extension/ui"
  local PIN="ded075b"
  if [[ ! -f "$UI_DIR/CMakeLists.txt" ]]; then
    echo "ui: vendoring duckdb-ui @ $PIN" >&2
    local tmp="$BUILD_DIR/duckdb-ui-src"
    if [[ ! -d "$tmp/.git" ]]; then
      git clone --quiet https://github.com/duckdb/duckdb-ui "$tmp"
    fi
    git -C "$tmp" checkout --quiet "$PIN"
    mkdir -p "$UI_DIR"
    ( cd "$tmp" && git archive "$PIN" CMakeLists.txt src third_party ) | tar -x -C "$UI_DIR"
  fi
  # Apply the wasm patches (httplib AF_UNIX/getnameinfo, watcher no-op, the request
  # bridge, system()-guard, CMakeLists). Idempotent (guard on the bridge marker).
  if ! grep -q 'duckdb_ui_handle_request' "$UI_DIR/src/http_server.cpp" 2>/dev/null; then
    for p in "$(pwd)"/cmake/ui-deps/ui-ded075b-*.patch; do
      ( cd "$UI_DIR" && git apply "$p" 2>/dev/null ) || patch -p1 -d "$UI_DIR" < "$p"
    done
    echo "ui: applied wasm patches" >&2
  fi
}

# delta: stage the prebuilt wasm kernel before configure (the vendored delta
# CMakeLists references the staged .a as an ExternalProject byproduct).
stage_delta_kernel
stage_aws_extension
stage_azure_extension
stage_ui_extension

# Configure, patching fetched sources after each failure, until it succeeds.
# Extensions are fetched progressively, so a configure-blocking extension only
# becomes patchable once the earlier-failing one is fixed (hence the loop).
attempt=0
until configure_duckdb; do
  attempt=$((attempt + 1))
  if [[ $attempt -ge 6 ]]; then
    echo "configure still failing after $attempt attempts" >&2; exit 1
  fi
  echo "configure attempt $attempt failed; patching fetched sources + retrying" >&2
  apply_extension_patches
done
# Final pass for compile-only source patches (httpfs CA bundle, curl default) on
# sources fetched in the successful configure.
apply_extension_patches

# TVM spill: route the buffer manager's temporary-block spill (evicted blocks +
# larger-than-memory sort/hash/aggregate) to host-owned Tiered Virtual Memory
# regions (cmake/.. crates/duckdb-core-component/src/tvm_spill.rs) instead of
# temp files -- extending capacity past the wasm32 4 GiB ceiling. The hooks fall
# back to temp files when no TVM host is wired, so this is always safe.
python3 - "$DUCKDB_SOURCE_DIR/src/storage/standard_buffer_manager.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'tvm_spill_write' in s:
    sys.exit(0)
# 1) extern "C" decls of the bridge (wasm-only)
decls = ('\n#ifdef __wasi__\n'
         'extern "C" {\n'
         '// TVM spill bridge (crates/duckdb-core-component/src/tvm_spill.rs).\n'
         'int tvm_spill_write(uint8_t tag, int64_t block_id, const uint8_t *data,\n'
         '                    uint64_t alloc_size, uint64_t logical_size, uint64_t header_size);\n'
         'int tvm_spill_query(int64_t block_id, uint64_t *out_logical, uint64_t *out_header);\n'
         'int tvm_spill_read(int64_t block_id, uint8_t *out, uint64_t capacity);\n'
         'uint64_t tvm_spill_delete(int64_t block_id);\n'
         '}\n#endif\n')
s = s.replace('namespace duckdb {\n', 'namespace duckdb {\n' + decls, 1)
# 2) WriteTemporaryBuffer -> TVM first
wanchor = 'void StandardBufferManager::WriteTemporaryBuffer(MemoryTag tag, block_id_t block_id, FileBuffer &buffer) {\n'
s = s.replace(wanchor, wanchor +
    '#ifdef __wasi__\n'
    '\tif (tvm_spill_write(uint8_t(tag), block_id, buffer.InternalBuffer(), buffer.AllocSize(),\n'
    '\t                    buffer.size, buffer.GetHeaderSize())) {\n'
    '\t\tevicted_data_per_tag[uint8_t(tag)] += buffer.AllocSize();\n'
    '\t\treturn;\n'
    '\t}\n#endif\n', 1)
# 3) ReadTemporaryBuffer -> TVM first
ranchor = 'unique_ptr<FileBuffer> reusable_buffer) {\n\tD_ASSERT(!temporary_directory.path.empty());'
s = s.replace(ranchor,
    'unique_ptr<FileBuffer> reusable_buffer) {\n'
    '#ifdef __wasi__\n'
    '\t{\n'
    '\t\tuint64_t tvm_logical = 0, tvm_header = 0;\n'
    '\t\tif (tvm_spill_query(block.BlockId(), &tvm_logical, &tvm_header)) {\n'
    '\t\t\tauto tvm_buffer = ConstructManagedBuffer(tvm_logical, tvm_header, std::move(reusable_buffer));\n'
    '\t\t\ttvm_spill_read(block.BlockId(), tvm_buffer->InternalBuffer(), tvm_buffer->AllocSize());\n'
    '\t\t\treturn tvm_buffer;\n'
    '\t\t}\n'
    '\t}\n#endif\n'
    '\tD_ASSERT(!temporary_directory.path.empty());', 1)
# 4) DeleteTemporaryFile -> TVM first
danchor = 'void StandardBufferManager::DeleteTemporaryFile(BlockHandle &block) {\n\tauto id = block.BlockId();\n'
s = s.replace(danchor, danchor +
    '#ifdef __wasi__\n'
    '\t{\n'
    '\t\tuint64_t tvm_freed = tvm_spill_delete(id);\n'
    '\t\tif (tvm_freed) {\n'
    '\t\t\tevicted_data_per_tag[uint8_t(block.GetMemoryTag())] -= tvm_freed;\n'
    '\t\t\treturn;\n'
    '\t\t}\n'
    '\t}\n#endif\n', 1)
open(p, 'w').write(s)
print('patched standard_buffer_manager.cpp: TVM spill hooks')
PY

# TVM spill, part 2: make a temp block evictable when a TVM host is wired even
# with no temporary directory. BlockHandle::CanUnload otherwise refuses to unload
# such a block (no spill target), which surfaces as "Unused blocks cannot be
# offloaded to disk" before WriteTemporaryBuffer (and its TVM hook) is reached.
python3 - "$DUCKDB_SOURCE_DIR/src/storage/buffer/block_handle.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'tvm_spill_available' in s:
    sys.exit(0)
decl = ('\n#ifdef __wasi__\n'
        '// TVM spill bridge (crates/duckdb-core-component/src/tvm_spill.rs).\n'
        'extern "C" int tvm_spill_available();\n'
        '#endif\n')
s = s.replace('namespace duckdb {\n', 'namespace duckdb {\n' + decl, 1)
anchor = ('\tif (block_id >= MAXIMUM_BLOCK && MustWriteToTemporaryFile() &&\n'
          '\t    !block_manager.buffer_manager.HasTemporaryDirectory()) {')
repl = ('\tif (block_id >= MAXIMUM_BLOCK && MustWriteToTemporaryFile() &&\n'
        '\t    !block_manager.buffer_manager.HasTemporaryDirectory()\n'
        '#ifdef __wasi__\n'
        '\t    && !tvm_spill_available()\n'
        '#endif\n'
        '\t    ) {')
if anchor not in s:
    sys.exit('block_handle.cpp: CanUnload anchor not found')
s = s.replace(anchor, repl, 1)
open(p, 'w').write(s)
print('patched block_handle.cpp: TVM makes temp blocks evictable without a temp dir')
PY

echo "Building libduckdb static archive" >&2
cmake --build "$BUILD_DIR" --target duckdb_static

# The extension config (cmake/wasm-extension-config.cmake) defines a
# `sqlite_wasivfs` static lib (the WASI VFS + sqlite3_os_init backing
# sqlite_scanner's vendored sqlite3.c). Build it and merge it below.
WASIVFS_LIB=""
if cmake --build "$BUILD_DIR" --target sqlite_wasivfs >&2; then
  WASIVFS_LIB="$(find "$BUILD_DIR" -name 'libsqlite_wasivfs.a' -print -quit)"
fi

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
ADDLIBS=$'ADDLIB libduckdb_base.a\nADDLIB libc++abi.a\nADDLIB libc++.a\nADDLIB libunwind.a'
if [[ -n "$WASIVFS_LIB" && -f "$WASIVFS_LIB" ]]; then
  cp "$WASIVFS_LIB" "$TMPDIR/libsqlite_wasivfs.a"
  ADDLIBS="$ADDLIBS"$'\nADDLIB libsqlite_wasivfs.a'
  echo "Merging WASI VFS ($WASIVFS_LIB) into libduckdb-wasi.a" >&2
fi

# httpfs links openssl (openssl-wasm: socket-capable) + libcurl/zlib/zstd
# (curl-wasm). Merge them so the core resolves SSL/EVP/curl/inflate symbols. One
# openssl (openssl-wasm); curl's openssl symbols resolve from it. Only when httpfs
# is enabled in the config; harmless if the libs are absent.
OPENSSL_WASM_BUILD="${OPENSSL_WASM_BUILD:-$HOME/git/openssl-wasm/build/openssl}"
CURL_WASM_BUILD="${CURL_WASM_BUILD:-$HOME/git/curl-wasm/build}"
if grep -q "duckdb_extension_load(httpfs" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  # curl-wasm is built with HTTP/2 (nghttp2) + HTTP/3 (USE_NGTCP2: ngtcp2 +
  # nghttp3). Merge those too, alongside openssl/zlib/zstd/brotli.
  for src in "$OPENSSL_WASM_BUILD/libssl.a" "$OPENSSL_WASM_BUILD/libcrypto.a" \
             "$CURL_WASM_BUILD/curl/lib/libcurl.a" "$CURL_WASM_BUILD/zlib/lib/libz.a" \
             "$CURL_WASM_BUILD/zstd/lib/libzstd.a" \
             "$CURL_WASM_BUILD/brotli/lib/libbrotlidec.a" \
             "$CURL_WASM_BUILD/brotli/lib/libbrotlicommon.a" \
             "$CURL_WASM_BUILD/nghttp2/lib/libnghttp2.a" \
             "$CURL_WASM_BUILD/ngtcp2/lib/libngtcp2.a" \
             "$CURL_WASM_BUILD/ngtcp2/lib/libngtcp2_crypto_ossl.a" \
             "$CURL_WASM_BUILD/nghttp3/lib/libnghttp3.a"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging httpfs dep ($src) into libduckdb-wasi.a" >&2
    fi
  done

  # cargo-component builds the core as a wasm32-wasip1 module (+ p1->p2 adapter),
  # and the wasip1 libc has NO BSD sockets. openssl-wasm/curl/httplib call
  # socket/connect/bind/...; those live only in the wasm32-wasip2 libc as thin
  # shims over wasi:sockets. Graft the exact socket objects PLUS the generated
  # component-binding glue they call (descriptor_table.c.obj + wasip2.c.obj, which
  # provide poll_poll / streams_method_* / network_* / monotonic_clock_* / list
  # helpers). These import wasi:sockets/io/clocks directly; wit-component surfaces
  # those imports on the final component (host grants them via inherit_network()).
  WASIP2_LIBC="$WASI_SDK_PREFIX/share/wasi-sysroot/lib/wasm32-wasip2/libc.a"
  if [[ -f "$WASIP2_LIBC" ]]; then
    SOCKDIR="$TMPDIR/wasip2-sockets"; mkdir -p "$SOCKDIR"
    # socket surface + the generated component bindings (wasip2.c.obj) + their
    # transitive deps: descriptor_table, wasip2 stdio-over-streams (wasip2_stdio,
    # file_utils), and wasip2_component_type.o (defines the force-link marker that
    # wasip2.c.obj references; carries the wasi import type info wit-component
    # reads when componentizing).
    SOCK_OBJS="socket.c.obj connect.c.obj bind.c.obj listen.c.obj accept.c.obj \
      getsockpeername.c.obj sockopt.c.obj netdb.c.obj recv.c.obj send.c.obj \
      recvfrom.c.obj sendto.c.obj recvmsg.c.obj sendmsg.c.obj shutdown.c.obj \
      socketpair.c.obj sockets_utils.c.obj tcp.c.obj udp.c.obj poll.c.obj \
      descriptor_table.c.obj wasip2.c.obj wasip2_stdio.c.obj file_utils.c.obj \
      wasip2_component_type.o"
    avail=""
    for o in $SOCK_OBJS; do
      "$WASI_SDK_PREFIX/bin/llvm-ar" t "$WASIP2_LIBC" 2>/dev/null | grep -qx "$o" && avail="$avail $o"
    done
    if [[ -n "$avail" ]]; then
      ( cd "$SOCKDIR" && "$WASI_SDK_PREFIX/bin/llvm-ar" x "$WASIP2_LIBC" $avail )
      # extracted members end in .obj AND .o (wasip2_component_type.o) -> glob both
      "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libwasip2sockets.a" "$SOCKDIR"/*
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libwasip2sockets.a"
      echo "Merging wasip2 socket+binding objects ($(echo $avail | wc -w | tr -d ' ') objs) into libduckdb-wasi.a" >&2
    fi
  fi

  # openssl-wasm seeds its RNG with getpid(); wasi libc has no getpid. Provide a
  # fixed-value stub (getpid is only mixed into entropy, not a security source on
  # a single-process wasm sandbox). Compiled for the same target as the archive.
  printf 'int getpid(void){return 42;}\n' > "$TMPDIR/wasi_getpid.c"
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" \
    -O2 -c "$TMPDIR/wasi_getpid.c" -o "$TMPDIR/wasi_getpid.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libwasigetpid.a" "$TMPDIR/wasi_getpid.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libwasigetpid.a"
  echo "Merging getpid stub into libduckdb-wasi.a" >&2
fi

# avro extension links libavro + libjansson (deflate codec uses zlib, already
# merged with httpfs). iceberg links libroaring. Merge the wasi deps built by
# scripts/build-wasi-deps.sh so the core resolves their symbols.
WASI_DEPS="${WASI_DEPS:-$(pwd)/build/wasi-deps}"
if grep -q "duckdb_extension_load(avro" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  deps=("$WASI_DEPS/avro-c/lib/libavro.a" "$WASI_DEPS/jansson/lib/libjansson.a" \
        "$WASI_DEPS/snappy/lib/libsnappy.a" "$WASI_DEPS/lzma/lib/liblzma.a")
  # zlib (deflate codec) -- only if httpfs didn't already merge it
  grep -q "duckdb_extension_load(httpfs" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
    || deps+=("$HOME/git/curl-wasm/build/zlib/lib/libz.a")
  grep -q "duckdb_extension_load(iceberg" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
    && deps+=("$WASI_DEPS/roaring/lib/libroaring.a")
  for src in "${deps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging avro/iceberg dep ($src) into libduckdb-wasi.a" >&2
    fi
  done
fi

# delta: merge the prebuilt delta-kernel-rs FFI staticlib (sync engine). CMake
# links it to the extension target via target_link_libraries, but a static lib
# doesn't bundle its link deps, so the kernel symbols must be merged into the
# combined archive the core links against (same as avro/iceberg/spatial).
if grep -q "duckdb_extension_load(delta" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  KERNEL_A="$(pwd)/build/delta-kernel/out/libdelta_kernel_ffi.a"
  if [[ -f "$KERNEL_A" ]]; then
    cp "$KERNEL_A" "$TMPDIR/libdelta_kernel_ffi.a"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libdelta_kernel_ffi.a"
    echo "Merging delta kernel ($KERNEL_A) into libduckdb-wasi.a" >&2
  else
    echo "WARNING: delta enabled but kernel missing: $KERNEL_A" >&2
  fi
fi

# azure: merge the prebuilt Azure SDK for C++ (azure-core + storage-common/blobs/
# datalake + identity) and its libxml2 dep. The extension links these via the
# Azure:: targets on native, but on wasm they're separate static libs that must
# be in the combined archive the core links against. curl-wasm + openssl-wasm
# (the transport + crypto) are already merged for httpfs.
if grep -q "duckdb_extension_load(azure" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  AZURE_LIB="$(pwd)/build/azure-sdk/out/lib"
  azure_deps=("$AZURE_LIB/libazure-storage-files-datalake.a" "$AZURE_LIB/libazure-storage-blobs.a"
              "$AZURE_LIB/libazure-storage-common.a" "$AZURE_LIB/libazure-identity.a"
              "$AZURE_LIB/libazure-core.a"
              "${LIBXML2_WASM:-$HOME/git/libxml2-wasm/build/install}/lib/libxml2.a")
  for src in "${azure_deps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging azure dep ($src) into libduckdb-wasi.a" >&2
    else
      echo "WARNING: azure enabled but dep missing: $src" >&2
    fi
  done
fi

# spatial: merge the geo stack (GEOS + PROJ + GDAL + tiff/jpeg/png/expat/sqlite +
# proj data) from the ~/git/*-wasm libs, plus a stubs lib for the ~24 wasi-missing
# symbols GDAL references (dlopen/fork/exec/sqlite-extras). zlib comes from httpfs.
if grep -q "duckdb_extension_load(spatial" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  # compile the weak stubs
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
    -c "$(pwd)/cmake/spatial-deps/stubs.c" -o "$TMPDIR/spatial_stubs.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libspatialstubs.a" "$TMPDIR/spatial_stubs.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libspatialstubs.a"
  # libjpeg uses setjmp/longjmp -> the wasm-sjlj runtime (__wasm_setjmp/longjmp)
  SJLJ="$WASI_SDK_PREFIX/share/wasi-sysroot/lib/${WASI_TARGET_TRIPLE:-wasm32-wasip2}/libsetjmp.a"
  if [[ -f "$SJLJ" ]]; then
    cp "$SJLJ" "$TMPDIR/libsetjmp.a"; ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libsetjmp.a"
    echo "Merging libsetjmp (wasm sjlj runtime) into libduckdb-wasi.a" >&2
  fi
  # PROJ: use the build_real_sqlite variant (real sqlite + memvfs-embedded
  # proj.db, no runtime files). libproj.a references pj_get_embedded_proj_db
  # which lives in a separate proj_resources object -> bundle it as a lib.
  PROJ_RS="$HOME/git/proj-wasm/build_real_sqlite/deps/proj"
  PROJ_RES_OBJ="$PROJ_RS/src/CMakeFiles/proj_resources.dir/embedded_resources.c.obj"
  if [[ -f "$PROJ_RES_OBJ" ]]; then
    "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libprojresources.a" "$PROJ_RES_OBJ"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libprojresources.a"
  fi
  # sqlite3: compile proj-wasm's 3.45.0 amalgamation with SQLITE_USE_URI=1 (the
  # spatial extension's proj_module opens the embedded proj.db via a memvfs
  # `file:?ptr=` URI without passing SQLITE_OPEN_URI, so URI parsing must be
  # compiled in). Matches proj-wasm's wasi flags otherwise. This .obj wins the
  # final merge (added last) over sqlite_scanner's 3.38.1, so all sqlite3
  # callers (proj, memvfs, gdal, sqlite_scanner) share one URI-enabled 3.45.0.
  SQLITE_SRC="$HOME/git/proj-wasm/deps/sqlite/sqlite3.c"
  if [[ -f "$SQLITE_SRC" ]]; then
    "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
      -DSQLITE_USE_URI=1 -DSQLITE_OMIT_WAL=1 -DSQLITE_OMIT_LOAD_EXTENSION=1 \
      -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_SHARED_CACHE=1 -DSQLITE_DEFAULT_MEMSTATUS=0 \
      -DSQLITE_LIKE_DOESNT_MATCH_BLOBS=1 -DSQLITE_OMIT_DEPRECATED=1 -DSQLITE_USE_ALLOCA=1 \
      -DSQLITE_OMIT_AUTOINIT=1 -DSQLITE_OMIT_POSIX_ADVISORY_LOCKING=1 \
      -c "$SQLITE_SRC" -o "$TMPDIR/sqlite3.c.obj"
    "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libsqlite3uri.a" "$TMPDIR/sqlite3.c.obj"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libsqlite3uri.a"
    echo "Merging URI-enabled sqlite3 (3.45.0) into libduckdb-wasi.a" >&2
  fi
  geo=("$HOME/git/gdal-wasm/build/deps/gdal/libgdal.a"
       "$HOME/git/geos-wasm/lib/lib/libgeos_c.a" "$HOME/git/geos-wasm/lib/lib/libgeos.a"
       "$PROJ_RS/lib/libproj.a"
       "$HOME/git/libtiff-wasm/build/lib/libtiff.a"
       "$HOME/git/libjpeg-turbo-wasm/build/libjpeg-turbo/libjpeg.a"
       "$HOME/git/libpng-wasm/build-wasip1/lib/libpng16.a"
       "$HOME/git/expat-wasm/build/lib/libexpat.a")
  for src in "${geo[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging spatial geo dep ($src) into libduckdb-wasi.a" >&2
    else
      echo "WARNING: spatial geo dep missing: $src" >&2
    fi
  done
fi

# excel: xlsx = zip(expat-parsed XML). Merge minizip-ng + expat + zlib (the
# latter two only if not already merged by spatial/httpfs/avro).
if grep -q "duckdb_extension_load(excel" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  WASI_DEPS="${WASI_DEPS:-$(pwd)/build/wasi-deps}"
  xdeps=("$WASI_DEPS/minizip/lib/libminizip-ng.a")
  grep -q "duckdb_extension_load(spatial" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
    || xdeps+=("$HOME/git/expat-wasm/build/lib/libexpat.a")
  grep -qE "duckdb_extension_load\((spatial|httpfs|avro)" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null \
    || xdeps+=("$HOME/git/curl-wasm/build/zlib/lib/libz.a")
  for src in "${xdeps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging excel dep ($src) into libduckdb-wasi.a" >&2
    else
      echo "WARNING: excel dep missing: $src" >&2
    fi
  done
fi

# postgres_scanner / mysql_scanner: the pg-wasi posix stubs (no-op signal API +
# getpwuid/getuid/popen + gai_strerror) + the getaddrinfo wrapper (numeric IPs
# resolve locally; wasi's getaddrinfo rejects them via resolve-addresses).
# Sockets + openssl come from httpfs's wasip2 graft, so both DB scanners require
# httpfs. postgres compiles libpq inline; mysql merges a prebuilt libmariadb.
if grep -qE "duckdb_extension_load\((postgres|mysql)_scanner" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
    -c "$(pwd)/cmake/postgres-wasi/stubs.c" -o "$TMPDIR/pg_stubs.o" \
    -I"$(pwd)/cmake/postgres-wasi/include"
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
    -c "$(pwd)/cmake/postgres-wasi/getaddrinfo_wrap.c" -o "$TMPDIR/pg_gaiwrap.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libpgstubs.a" "$TMPDIR/pg_stubs.o" "$TMPDIR/pg_gaiwrap.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libpgstubs.a"
  echo "Merging pg-wasi stubs + getaddrinfo wrapper into libduckdb-wasi.a" >&2
fi
# mysql_scanner: merge the prebuilt MariaDB Connector/C (libpq is inline; this
# is the equivalent for mysql).
if grep -q "duckdb_extension_load(mysql_scanner" "$DUCKDB_EXTENSION_CONFIGS" 2>/dev/null; then
  WASI_DEPS="${WASI_DEPS:-$(pwd)/build/wasi-deps}"
  if [[ -f "$WASI_DEPS/mariadb/lib/mariadb/libmariadbclient.a" ]]; then
    cp "$WASI_DEPS/mariadb/lib/mariadb/libmariadbclient.a" "$TMPDIR/libmariadbclient.a"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libmariadbclient.a"
    echo "Merging libmariadbclient into libduckdb-wasi.a" >&2
  else
    echo "WARNING: mysql dep missing: $WASI_DEPS/mariadb/lib/mariadb/libmariadbclient.a" >&2
  fi
fi
pushd "$TMPDIR" >/dev/null
printf 'CREATE libduckdb_combined.a\n%s\nSAVE\nEND\n' "$ADDLIBS" | "$WASI_SDK_PREFIX/bin/llvm-ar" -M
popd >/dev/null

cp "$TMPDIR/libduckdb_combined.a" "$ARTIFACTS_DIR/libduckdb-wasi.a"

echo "Static library copied to $ARTIFACTS_DIR/libduckdb-wasi.a" >&2
