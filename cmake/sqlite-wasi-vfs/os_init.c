/*
 * SQLITE_OS_OTHER hooks for the wasm32-wasi build of DuckDB's sqlite_scanner.
 *
 * sqlite_scanner vendors sqlite3.c, whose unix VFS references syscalls wasi
 * libc omits (struct unix_syscall[] is incomplete -> compile error). We compile
 * that sqlite3.c with -DSQLITE_OS_OTHER=1, which drops the built-in unix/win OS
 * layer and instead requires the application to provide sqlite3_os_init() /
 * sqlite3_os_end(). sqlite3_initialize() (called lazily on the first sqlite3 API
 * use, e.g. sqlite_scanner's sqlite3_open_v2) invokes sqlite3_os_init(), so this
 * is where we register the WASI VFS.
 *
 * The VFS (vfs_wasi.c) is reused from ~/git/sqlite-wasm; it bridges sqlite3's
 * VFS callbacks onto WASI filesystem syscalls so a file-backed open under a
 * host-preopened directory reads the database file from disk.
 *
 * We register wasivfs as the DEFAULT (makeDefault=1) so sqlite_scanner's opens
 * (which pass a NULL vfs name) pick it up. Unlike the sqlite-wasm core (which
 * defaults to an in-memory memvfs), here DuckDB owns in-memory storage and we
 * only need sqlite for reading on-disk .sqlite files.
 */
#include "sqlite3.h"

extern int sqlite3_wasivfs_register(int makeDefault);

int sqlite3_os_init(void) {
    return sqlite3_wasivfs_register(1);
}

int sqlite3_os_end(void) {
    return SQLITE_OK;
}
