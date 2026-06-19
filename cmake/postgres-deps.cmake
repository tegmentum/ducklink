# Deps for the duckdb-postgres extension on wasi, replacing its
# find_package(OpenSSL). The pinned extension (f012a4f) compiles libpq's sources
# INLINE (LIBPG_SOURCES) from a downloaded PostgreSQL tree rather than linking a
# prebuilt libpq. scripts/build-libduckdb-wasm.sh stages a wasi-cross-configured
# PostgreSQL 15.13 source as that `postgres/` tree (built by build-wasi-deps.sh)
# and drops the getaddrinfo/gettimeofday fallback files (wasi provides those).
#
# Here we (1) force-include cmake/postgres-wasi/shim.h into the compile for the
# posix gaps wasi-libc omits (sigaction/pwd/termios/sockaddr_un) -- it also
# carries a minimal pg_config_paths.h -- and (2) provide OpenSSL from
# openssl-wasm so TLS works (fe-secure-openssl.c compiles + links against it).
# Sockets come from httpfs's wasip2 graft; cmake/postgres-wasi/stubs.c (merged
# into libduckdb-wasi.a) supplies the no-op signal/uid implementations.
get_filename_component(_PGW "${CMAKE_CURRENT_LIST_DIR}/postgres-wasi" ABSOLUTE)
# PG_WASI_REAL_NETDB makes cmake/wasi-override/include/netdb.h defer to the real
# wasi <netdb.h> (real getaddrinfo, wrappable for numeric IPs) instead of the
# no-socket stub the duckdb toolchain uses for core builds.
add_compile_options("-include${_PGW}/shim.h" "-I${_PGW}/include"
                    "-DPG_WASI_REAL_NETDB" "-Wno-unused-command-line-argument")

# --- OpenSSL (openssl-wasm; shared with httpfs -> guard) ------------------
set(_OSSL "$ENV{HOME}/git/openssl-wasm/build/openssl")
set(_OSSL_INC "${_OSSL}/include;$ENV{HOME}/git/openssl-wasm/third_party/openssl/include")
if(NOT TARGET OpenSSL::SSL)
  add_library(OpenSSL::SSL STATIC IMPORTED GLOBAL)
  set_target_properties(OpenSSL::SSL PROPERTIES
    IMPORTED_LOCATION "${_OSSL}/libssl.a"
    INTERFACE_INCLUDE_DIRECTORIES "${_OSSL_INC}")
endif()
if(NOT TARGET OpenSSL::Crypto)
  add_library(OpenSSL::Crypto STATIC IMPORTED GLOBAL)
  set_target_properties(OpenSSL::Crypto PROPERTIES
    IMPORTED_LOCATION "${_OSSL}/libcrypto.a"
    INTERFACE_INCLUDE_DIRECTORIES "${_OSSL_INC}")
endif()
set(OPENSSL_FOUND TRUE)
set(OPENSSL_INCLUDE_DIR "${_OSSL_INC}")
set(OPENSSL_SSL_LIBRARY "${_OSSL}/libssl.a")
set(OPENSSL_CRYPTO_LIBRARY "${_OSSL}/libcrypto.a")
set(OPENSSL_LIBRARIES OpenSSL::SSL OpenSSL::Crypto)
