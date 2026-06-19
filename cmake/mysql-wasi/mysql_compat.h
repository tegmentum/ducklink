/* MariaDB Connector/C lacks MySQL's `enum mysql_ssl_mode` + MYSQL_OPT_SSL_MODE
   that the duckdb-mysql extension uses. Provide them (force-included before
   <mysql.h> via cmake/mysql-deps.cmake). The default ssl_mode (PREFERRED) path
   doesn't call mysql_options(MYSQL_OPT_SSL_MODE), so this affects only
   compilation + non-default ssl modes -- where MariaDB ignores the unknown
   option and applies its own SSL default. MYSQL_OPT_SSL_MODE sits above
   MariaDB's option enum range (~7030) so it can't collide. */
#ifndef DUCKDB_MYSQL_WASI_COMPAT_H
#define DUCKDB_MYSQL_WASI_COMPAT_H
#ifdef __wasi__
enum mysql_ssl_mode {
    SSL_MODE_DISABLED = 1,
    SSL_MODE_PREFERRED,
    SSL_MODE_REQUIRED,
    SSL_MODE_VERIFY_CA,
    SSL_MODE_VERIFY_IDENTITY
};
#define MYSQL_OPT_SSL_MODE ((enum mysql_option)8000)
#endif
#endif
