// wasi stubs for symbols GDAL references but wasi-libc / the wasm sqlite lack.
// All weak, so a real implementation (e.g. wasi-emulated-signal's signal,
// the httpfs getpid stub) wins; these only fill genuine gaps. GDAL degrades
// gracefully when these fail (no dynamic drivers, no subprocess, no sqlite
// column metadata / extension loading) — fine for the core ST_* + format I/O.
#include <stddef.h>
#include <errno.h>

#define WEAK __attribute__((weak))

// --- dynamic loading (GDAL plugin drivers; none on wasi) ---
WEAK void *dlopen(const char *f, int flag) { (void)f; (void)flag; return NULL; }
WEAK void *dlsym(void *h, const char *s) { (void)h; (void)s; return NULL; }
WEAK int dlclose(void *h) { (void)h; return 0; }
WEAK char *dlerror(void) { return (char *)"dlopen unsupported on wasi"; }

// --- process spawning (GDAL /vsi pipe drivers; unsupported on wasi) ---
WEAK int fork(void) { errno = ENOSYS; return -1; }
WEAK int execvp(const char *file, char *const argv[]) { (void)file; (void)argv; errno = ENOSYS; return -1; }
WEAK int pipe(int fds[2]) { (void)fds; errno = ENOSYS; return -1; }
WEAK int dup2(int oldfd, int newfd) { (void)oldfd; (void)newfd; errno = ENOSYS; return -1; }

// --- threading no-ops (single-threaded component) ---
WEAK int pthread_atfork(void (*a)(void), void (*b)(void), void (*c)(void)) {
	(void)a; (void)b; (void)c; return 0;
}

// --- sqlite features the wasm sqlite was built without (GDAL's SQLite/GPKG
//     driver uses these; NULL/noop disables only the affected metadata) ---
WEAK const char *sqlite3_column_table_name(void *stmt, int col) { (void)stmt; (void)col; return NULL; }
WEAK const char *sqlite3_column_origin_name(void *stmt, int col) { (void)stmt; (void)col; return NULL; }
WEAK const char *sqlite3_column_database_name(void *stmt, int col) { (void)stmt; (void)col; return NULL; }
WEAK int sqlite3_enable_load_extension(void *db, int onoff) { (void)db; (void)onoff; return 0; }
WEAK int sqlite3_load_extension(void *db, const char *file, const char *proc, char **err) {
	(void)db; (void)file; (void)proc;
	if (err) { *err = NULL; }
	return 1; // SQLITE_ERROR
}
