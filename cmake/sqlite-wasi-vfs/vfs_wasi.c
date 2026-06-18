/*
 * SQLite WASM WASI VFS Implementation
 *
 * Provides a virtual file system backed by WASI filesystem APIs.
 * This allows SQLite to persist data to the host filesystem when running
 * in WASI-compatible runtimes like wasmtime.
 *
 * Note: WASI does not support file locking, so this VFS operates in
 * a serialized mode suitable for single-process access.
 */

#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include "sqlite3.h"

/* WASI-specific includes - available in wasi-sdk */
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>

/* Configuration */
#define WASIVFS_MAX_PATHNAME  512
#define WASIVFS_SECTOR_SIZE   4096

/* Forward declarations */
typedef struct WasiFile WasiFile;

/* WASI file structure */
struct WasiFile {
    sqlite3_file base;      /* Base class - must be first */
    int fd;                 /* File descriptor */
    char *path;             /* File path for debugging */
    int lock_level;         /* Current lock level (simulated) */
};

/* Forward declarations of VFS methods */
static int wasivfs_open(sqlite3_vfs*, const char*, sqlite3_file*, int, int*);
static int wasivfs_delete(sqlite3_vfs*, const char*, int);
static int wasivfs_access(sqlite3_vfs*, const char*, int, int*);
static int wasivfs_fullpathname(sqlite3_vfs*, const char*, int, char*);
static void *wasivfs_dlopen(sqlite3_vfs*, const char*);
static void wasivfs_dlerror(sqlite3_vfs*, int, char*);
static void (*wasivfs_dlsym(sqlite3_vfs*, void*, const char*))(void);
static void wasivfs_dlclose(sqlite3_vfs*, void*);
static int wasivfs_randomness(sqlite3_vfs*, int, char*);
static int wasivfs_sleep(sqlite3_vfs*, int);
static int wasivfs_currenttime(sqlite3_vfs*, double*);
static int wasivfs_getlasterror(sqlite3_vfs*, int, char*);
static int wasivfs_currenttimeint64(sqlite3_vfs*, sqlite3_int64*);

/* Forward declarations of file methods */
static int wasifile_close(sqlite3_file*);
static int wasifile_read(sqlite3_file*, void*, int, sqlite3_int64);
static int wasifile_write(sqlite3_file*, const void*, int, sqlite3_int64);
static int wasifile_truncate(sqlite3_file*, sqlite3_int64);
static int wasifile_sync(sqlite3_file*, int);
static int wasifile_filesize(sqlite3_file*, sqlite3_int64*);
static int wasifile_lock(sqlite3_file*, int);
static int wasifile_unlock(sqlite3_file*, int);
static int wasifile_checkreservedlock(sqlite3_file*, int*);
static int wasifile_filecontrol(sqlite3_file*, int, void*);
static int wasifile_sectorsize(sqlite3_file*);
static int wasifile_devicecharacteristics(sqlite3_file*);

/* I/O methods structure */
static const sqlite3_io_methods g_wasifile_methods = {
    1,                          /* iVersion */
    wasifile_close,
    wasifile_read,
    wasifile_write,
    wasifile_truncate,
    wasifile_sync,
    wasifile_filesize,
    wasifile_lock,
    wasifile_unlock,
    wasifile_checkreservedlock,
    wasifile_filecontrol,
    wasifile_sectorsize,
    wasifile_devicecharacteristics,
    /* Version 2+ methods */
    NULL,                       /* xShmMap */
    NULL,                       /* xShmLock */
    NULL,                       /* xShmBarrier */
    NULL,                       /* xShmUnmap */
    /* Version 3+ methods */
    NULL,                       /* xFetch */
    NULL                        /* xUnfetch */
};

/* Global VFS instance */
static sqlite3_vfs g_wasivfs;
static int g_last_errno = 0;

/*
 * File method implementations
 */

static int wasifile_close(sqlite3_file *pFile) {
    WasiFile *file = (WasiFile*)pFile;

    if (file->fd >= 0) {
        close(file->fd);
        file->fd = -1;
    }

    if (file->path) {
        sqlite3_free(file->path);
        file->path = NULL;
    }

    return SQLITE_OK;
}

static int wasifile_read(sqlite3_file *pFile, void *buf, int amt, sqlite3_int64 offset) {
    WasiFile *file = (WasiFile*)pFile;

    /* Seek to position */
    off_t pos = lseek(file->fd, (off_t)offset, SEEK_SET);
    if (pos == (off_t)-1) {
        g_last_errno = errno;
        return SQLITE_IOERR_SEEK;
    }

    /* Read data */
    ssize_t got = read(file->fd, buf, amt);
    if (got < 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_READ;
    }

    if (got < amt) {
        /* Zero-fill remainder */
        memset((char*)buf + got, 0, amt - got);
        return SQLITE_IOERR_SHORT_READ;
    }

    return SQLITE_OK;
}

static int wasifile_write(sqlite3_file *pFile, const void *buf, int amt, sqlite3_int64 offset) {
    WasiFile *file = (WasiFile*)pFile;

    /* Seek to position */
    off_t pos = lseek(file->fd, (off_t)offset, SEEK_SET);
    if (pos == (off_t)-1) {
        g_last_errno = errno;
        return SQLITE_IOERR_SEEK;
    }

    /* Write data */
    ssize_t written = write(file->fd, buf, amt);
    if (written != amt) {
        g_last_errno = errno;
        return SQLITE_IOERR_WRITE;
    }

    return SQLITE_OK;
}

static int wasifile_truncate(sqlite3_file *pFile, sqlite3_int64 size) {
    WasiFile *file = (WasiFile*)pFile;

    if (ftruncate(file->fd, (off_t)size) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_TRUNCATE;
    }

    return SQLITE_OK;
}

static int wasifile_sync(sqlite3_file *pFile, int flags) {
    WasiFile *file = (WasiFile*)pFile;
    (void)flags;

    if (fsync(file->fd) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_FSYNC;
    }

    return SQLITE_OK;
}

static int wasifile_filesize(sqlite3_file *pFile, sqlite3_int64 *pSize) {
    WasiFile *file = (WasiFile*)pFile;
    struct stat st;

    if (fstat(file->fd, &st) != 0) {
        g_last_errno = errno;
        return SQLITE_IOERR_FSTAT;
    }

    *pSize = (sqlite3_int64)st.st_size;
    return SQLITE_OK;
}

static int wasifile_lock(sqlite3_file *pFile, int level) {
    WasiFile *file = (WasiFile*)pFile;
    /*
     * WASI does not support file locking.
     * We simulate locks for single-process use.
     * Multi-process scenarios are not supported.
     */
    file->lock_level = level;
    return SQLITE_OK;
}

static int wasifile_unlock(sqlite3_file *pFile, int level) {
    WasiFile *file = (WasiFile*)pFile;
    file->lock_level = level;
    return SQLITE_OK;
}

static int wasifile_checkreservedlock(sqlite3_file *pFile, int *pResOut) {
    WasiFile *file = (WasiFile*)pFile;
    /* No other process can hold a lock in WASI */
    *pResOut = (file->lock_level > SQLITE_LOCK_SHARED) ? 1 : 0;
    return SQLITE_OK;
}

static int wasifile_filecontrol(sqlite3_file *pFile, int op, void *pArg) {
    (void)pFile;
    if (op == SQLITE_FCNTL_VFSNAME && pArg != NULL) {
        // sqlite3_mprintf the vfs name into *pArg per the
        // SQLITE_FCNTL_VFSNAME contract. The caller is responsible
        // for sqlite3_free'ing it.
        char **out = (char**)pArg;
        *out = sqlite3_mprintf("%s", "wasivfs");
        return SQLITE_OK;
    }
    return SQLITE_NOTFOUND;
}

static int wasifile_sectorsize(sqlite3_file *pFile) {
    (void)pFile;
    return WASIVFS_SECTOR_SIZE;
}

static int wasifile_devicecharacteristics(sqlite3_file *pFile) {
    (void)pFile;
    return SQLITE_IOCAP_UNDELETABLE_WHEN_OPEN;
}

/*
 * VFS method implementations
 */

static int wasivfs_open(sqlite3_vfs *pVfs, const char *zName, sqlite3_file *pFile,
                        int flags, int *pOutFlags) {
    (void)pVfs;
    WasiFile *file = (WasiFile*)pFile;
    int oflags = 0;
    int rc = SQLITE_OK;

    memset(file, 0, sizeof(WasiFile));
    file->fd = -1;
    file->lock_level = SQLITE_LOCK_NONE;

    /* Convert SQLite flags to POSIX flags */
    if (flags & SQLITE_OPEN_EXCLUSIVE) {
        oflags |= O_EXCL;
    }
    if (flags & SQLITE_OPEN_CREATE) {
        oflags |= O_CREAT;
    }
    if (flags & SQLITE_OPEN_READONLY) {
        oflags |= O_RDONLY;
    } else if (flags & SQLITE_OPEN_READWRITE) {
        oflags |= O_RDWR;
    }

    /* Handle temp/anonymous files */
    if (zName == NULL || zName[0] == '\0') {
        /* Create a temp file name */
        static int temp_counter = 0;
        char temp_name[64];
        sqlite3_snprintf(sizeof(temp_name), temp_name, "/tmp/sqlite_temp_%d", ++temp_counter);
        zName = temp_name;
        oflags |= O_CREAT | O_RDWR;
    }

    /* Open the file */
    file->fd = open(zName, oflags, 0644);
    if (file->fd < 0) {
        g_last_errno = errno;
        return SQLITE_CANTOPEN;
    }

    /* Store path for debugging */
    file->path = sqlite3_mprintf("%s", zName);

    /* Set up I/O methods */
    file->base.pMethods = &g_wasifile_methods;

    if (pOutFlags) {
        *pOutFlags = flags;
    }

    return rc;
}

static int wasivfs_delete(sqlite3_vfs *pVfs, const char *zName, int syncDir) {
    (void)pVfs;
    (void)syncDir;

    if (unlink(zName) != 0) {
        g_last_errno = errno;
        if (errno == ENOENT) {
            return SQLITE_OK; /* File doesn't exist - that's OK */
        }
        return SQLITE_IOERR_DELETE;
    }

    return SQLITE_OK;
}

static int wasivfs_access(sqlite3_vfs *pVfs, const char *zName, int flags, int *pResOut) {
    (void)pVfs;
    int mode = 0;

    switch (flags) {
        case SQLITE_ACCESS_EXISTS:
            mode = F_OK;
            break;
        case SQLITE_ACCESS_READ:
            mode = R_OK;
            break;
        case SQLITE_ACCESS_READWRITE:
            mode = R_OK | W_OK;
            break;
        default:
            *pResOut = 0;
            return SQLITE_OK;
    }

    *pResOut = (access(zName, mode) == 0) ? 1 : 0;
    return SQLITE_OK;
}

static int wasivfs_fullpathname(sqlite3_vfs *pVfs, const char *zName, int nOut, char *zOut) {
    (void)pVfs;

    if (zName == NULL) {
        zOut[0] = '\0';
        return SQLITE_OK;
    }

    /* For WASI, we just use the name as-is since we don't have getcwd */
    int len = (int)strlen(zName);
    if (len >= nOut) {
        return SQLITE_CANTOPEN;
    }

    memcpy(zOut, zName, len + 1);
    return SQLITE_OK;
}

static void *wasivfs_dlopen(sqlite3_vfs *pVfs, const char *zFilename) {
    (void)pVfs;
    (void)zFilename;
    return NULL;
}

static void wasivfs_dlerror(sqlite3_vfs *pVfs, int nByte, char *zErrMsg) {
    (void)pVfs;
    if (nByte > 0) {
        sqlite3_snprintf(nByte, zErrMsg, "Dynamic loading not supported in WASI");
    }
}

static void (*wasivfs_dlsym(sqlite3_vfs *pVfs, void *pHandle, const char *zSymbol))(void) {
    (void)pVfs;
    (void)pHandle;
    (void)zSymbol;
    return NULL;
}

static void wasivfs_dlclose(sqlite3_vfs *pVfs, void *pHandle) {
    (void)pVfs;
    (void)pHandle;
}

static int wasivfs_randomness(sqlite3_vfs *pVfs, int nByte, char *zOut) {
    (void)pVfs;

    /* Try to use /dev/urandom if available */
    int fd = open("/dev/urandom", O_RDONLY);
    if (fd >= 0) {
        ssize_t got = read(fd, zOut, nByte);
        close(fd);
        if (got == nByte) {
            return nByte;
        }
    }

    /* Fallback to time-based PRNG */
    static uint32_t seed = 0;
    if (seed == 0) {
        seed = (uint32_t)time(NULL);
    }

    for (int i = 0; i < nByte; i++) {
        seed = seed * 1103515245 + 12345;
        zOut[i] = (char)(seed >> 16);
    }

    return nByte;
}

static int wasivfs_sleep(sqlite3_vfs *pVfs, int microseconds) {
    (void)pVfs;

    /* WASI preview2 may support this via wasi:clocks/monotonic-clock */
    /* For now, just busy-wait approximately */
    volatile int i;
    for (i = 0; i < microseconds / 10; i++) {
        /* Busy wait */
    }

    return microseconds;
}

static int wasivfs_currenttime(sqlite3_vfs *pVfs, double *pTime) {
    (void)pVfs;

    time_t t = time(NULL);
    /* Convert Unix time to Julian day */
    *pTime = (double)t / 86400.0 + 2440587.5;
    return SQLITE_OK;
}

static int wasivfs_getlasterror(sqlite3_vfs *pVfs, int nBuf, char *zBuf) {
    (void)pVfs;

    if (nBuf > 0 && zBuf) {
        sqlite3_snprintf(nBuf, zBuf, "errno=%d", g_last_errno);
    }

    return g_last_errno;
}

static int wasivfs_currenttimeint64(sqlite3_vfs *pVfs, sqlite3_int64 *pTime) {
    (void)pVfs;

    time_t t = time(NULL);
    /* Convert to milliseconds since Julian epoch */
    *pTime = (sqlite3_int64)t * 1000LL + 210866760000000LL;
    return SQLITE_OK;
}

/*
 * Public API
 */

int sqlite3_wasivfs_register(int makeDefault) {
    static int initialized = 0;

    if (initialized) {
        return SQLITE_OK;
    }

    memset(&g_wasivfs, 0, sizeof(g_wasivfs));

    g_wasivfs.iVersion = 2;
    g_wasivfs.szOsFile = sizeof(WasiFile);
    g_wasivfs.mxPathname = WASIVFS_MAX_PATHNAME;
    g_wasivfs.pNext = NULL;
    g_wasivfs.zName = "wasivfs";
    g_wasivfs.pAppData = NULL;
    g_wasivfs.xOpen = wasivfs_open;
    g_wasivfs.xDelete = wasivfs_delete;
    g_wasivfs.xAccess = wasivfs_access;
    g_wasivfs.xFullPathname = wasivfs_fullpathname;
    g_wasivfs.xDlOpen = wasivfs_dlopen;
    g_wasivfs.xDlError = wasivfs_dlerror;
    g_wasivfs.xDlSym = wasivfs_dlsym;
    g_wasivfs.xDlClose = wasivfs_dlclose;
    g_wasivfs.xRandomness = wasivfs_randomness;
    g_wasivfs.xSleep = wasivfs_sleep;
    g_wasivfs.xCurrentTime = wasivfs_currenttime;
    g_wasivfs.xGetLastError = wasivfs_getlasterror;
    g_wasivfs.xCurrentTimeInt64 = wasivfs_currenttimeint64;

    int rc = sqlite3_vfs_register(&g_wasivfs, makeDefault);
    if (rc == SQLITE_OK) {
        initialized = 1;
    }

    return rc;
}

const char *sqlite3_wasivfs_name(void) {
    return "wasivfs";
}
