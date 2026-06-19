# Deps for the duckdb-mysql (mysql_scanner) extension on wasi, replacing its
# find_package(libmysql). Unlike postgres (which compiles libpq inline), mysql
# links a PREBUILT MariaDB Connector/C (libmariadbclient.a, built for wasi by
# scripts/build-wasi-deps.sh against openssl-wasm). The .a is merged into
# libduckdb-wasi.a at the core link; openssl + sockets come from httpfs's graft.
#
# Reuses the postgres networking infra: force-include the posix shim
# (signals/pwd/termios/sockaddr_un/getnameinfo) and PG_WASI_REAL_NETDB so the
# duckdb toolchain's wasi-override netdb.h defers to the real <netdb.h> (real
# getaddrinfo, wrappable for numeric IPs by cmake/postgres-wasi/getaddrinfo_wrap.c).
get_filename_component(_PGW "${CMAKE_CURRENT_LIST_DIR}/postgres-wasi" ABSOLUTE)
get_filename_component(_MYW "${CMAKE_CURRENT_LIST_DIR}/mysql-wasi" ABSOLUTE)
get_filename_component(_REPO "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)
# shim.h: posix gaps; mysql_compat.h: MySQL ssl_mode enum MariaDB lacks (before
# <mysql.h>); cmake/mysql-wasi/include: mysql_com.h/mysql_version.h shims ->
# MariaDB's mariadb_*.h; PG_WASI_REAL_NETDB: real getaddrinfo (wrappable).
add_compile_options("-include${_PGW}/shim.h" "-include${_MYW}/mysql_compat.h"
                    "-I${_PGW}/include" "-I${_MYW}/include" "-DPG_WASI_REAL_NETDB"
                    "-Wno-unused-command-line-argument")

# MariaDB Connector/C installs public headers under include/mariadb and the
# static lib under lib/mariadb.
set(_MARIADB "${_REPO}/build/wasi-deps/mariadb")
set(_MARIADB_INC "${_MARIADB}/include/mariadb")
set(_MARIADB_LIB "${_MARIADB}/lib/mariadb/libmariadbclient.a")
include_directories("${_MYW}/include" "${_MARIADB_INC}")
add_library(libmysql::libmysql STATIC IMPORTED GLOBAL)
set_target_properties(libmysql::libmysql PROPERTIES
  IMPORTED_LOCATION "${_MARIADB_LIB}"
  INTERFACE_INCLUDE_DIRECTORIES "${_MARIADB_INC}")
set(libmysql_FOUND TRUE)
set(MYSQL_FOUND TRUE)
set(MYSQL_LIBRARIES libmysql::libmysql)
set(MYSQL_LIBRARY "${_MARIADB_LIB}")
set(MYSQL_INCLUDE_DIR "${_MARIADB_INC}")
