#pragma once

#ifdef __wasi__

#include <errno.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>

// WASI libc omits the POSIX file-lock constants; provide harmless stand-ins so
// DuckDB's file locking logic can compile. They map to zero so that any
// runtime use becomes a no-op in environments that do not support advisory
// locks.
#ifndef F_RDLCK
#define F_RDLCK 0
#endif
#ifndef F_WRLCK
#define F_WRLCK 1
#endif
#ifndef F_UNLCK
#define F_UNLCK 2
#endif
#ifndef F_SETLK
#define F_SETLK 3
#endif
#ifndef F_SETLKW
#define F_SETLKW 4
#endif
#ifndef F_GETLK
#define F_GETLK 5
#endif

// Minimal winsize definition so code paths that inspect the terminal size can
// compile even though WASI does not expose the ioctl. The struct layout matches
// the POSIX definition.
#ifndef __wasi_winsize_defined
#define __wasi_winsize_defined 1
struct winsize {
    unsigned short ws_row;
    unsigned short ws_col;
    unsigned short ws_xpixel;
    unsigned short ws_ypixel;
};
#endif

#ifndef TIOCGWINSZ
#define TIOCGWINSZ 0
#endif

// The WASI libc omits mlock/munlock; DuckDB uses them only as best-effort
// hardening. Implement them as benign stubs.
static inline int mlock(const void *, size_t) {
    return 0;
}

static inline int munlock(const void *, size_t) {
    return 0;
}

// Provide no-op fcntl/ioctl implementations that report ENOSYS so DuckDB can
// gracefully handle the missing functionality at runtime.
static inline int fcntl(int, int, ...) {
    errno = ENOSYS;
    return -1;
}

static inline int ioctl(int, unsigned long, ...) {
    errno = ENOTTY;
    return -1;
}

// WASI libc omits the POSIX `tzname` global, which ICU's putil.cpp references in
// its timezone-detection *fallback*. ICU checks getenv("TZ") first (the real
// default-zone source on wasm), and `SET TimeZone='…'` overrides per connection,
// so this stub is only a never-meaningfully-used placeholder to satisfy the
// reference. File-scope `static` keeps it per-translation-unit (no global symbol
// to collide/merge); `unused` silences the warning in the many non-ICU TUs that
// force-include this header.
static char *tzname[2] __attribute__((unused)) = { (char *) "UTC", (char *) "UTC" };

// WASI's <netdb.h> omits some getaddrinfo/getnameinfo flag constants that
// third_party/httplib uses (the httpfs HTTP client). Standard POSIX values;
// guarded so a future wasi-libc that adds them wins.
#ifndef AI_NUMERICHOST
#define AI_NUMERICHOST 0x00000004
#endif
#ifndef NI_MAXHOST
#define NI_MAXHOST 1025
#endif
#ifndef NI_NUMERICHOST
#define NI_NUMERICHOST 0x01
#endif

#endif // __wasi__
