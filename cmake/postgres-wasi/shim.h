/* wasi compat shim for building PostgreSQL libpq. Force-included into every TU.
   wasi-libc declares the POSIX signal API but #ifdef's it out ("WASI has no
   sigaction"); libpq only uses it to block SIGPIPE around socket writes, which
   is a no-op on wasi (wasi:sockets never raises signals). Provide the decls +
   missing errno here; pg-wasi/stubs.c provides no-op implementations. */
#ifndef PG_WASI_SHIM_H
#define PG_WASI_SHIM_H
#ifdef __wasi__
/* wasi's <signal.h> guards out sigset_t (no signals); pull it in directly the
   musl way (idempotent — bits/alltypes.h guards each type). */
#define __NEED_sigset_t
#include <bits/alltypes.h>
#include <errno.h>

#ifndef SA_RESTART
#define SA_RESTART   0x10000000
#endif
#ifndef SA_NOCLDSTOP
#define SA_NOCLDSTOP 0x00000001
#endif
#ifndef SIG_BLOCK
#define SIG_BLOCK   0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2
#endif
#ifndef EHOSTDOWN
#define EHOSTDOWN 112
#endif

/* struct sigaction is guarded out in wasi's <signal.h>; define what libpq uses.
   Function decls are extern "C" so the C stubs resolve when this is force-
   included into the extension's C++ TUs too. */
#ifdef __cplusplus
extern "C" {
#endif
#ifndef PG_WASI_HAVE_SIGACTION
#define PG_WASI_HAVE_SIGACTION
struct sigaction {
    void (*sa_handler)(int);
    sigset_t sa_mask;
    int sa_flags;
};
int sigaction(int, const struct sigaction *, struct sigaction *);
int sigemptyset(sigset_t *);
int sigfillset(sigset_t *);
int sigaddset(sigset_t *, int);
int sigdelset(sigset_t *, int);
int sigismember(const sigset_t *, int);
int sigpending(sigset_t *);
int sigprocmask(int, const sigset_t *, sigset_t *);
int pthread_sigmask(int, const sigset_t *, sigset_t *);
int sigwait(const sigset_t *, int *);
#endif
#ifdef __cplusplus
}
#endif

/* wasi's struct sockaddr_un stub has only sun_family (no UNIX sockets). Pre-empt
   its include guard to supply the full struct so libpq's local-socket code
   compiles; unix connections fail at runtime, we only use TCP. */
#ifndef __wasilibc___struct_sockaddr_un_h
#define __wasilibc___struct_sockaddr_un_h
#include <__typedef_sa_family_t.h>
struct sockaddr_un {
    sa_family_t sun_family;
    char sun_path[108];
};
#endif

/* getuid/geteuid/popen/pclose are guarded out on wasi; declare + stub them */
#ifndef PG_WASI_HAVE_UNISTD_EXTRAS
#define PG_WASI_HAVE_UNISTD_EXTRAS
#include <sys/types.h>
#include <stdio.h>
#ifdef __cplusplus
extern "C" {
#endif
uid_t getuid(void);
uid_t geteuid(void);
FILE *popen(const char *, const char *);
int pclose(FILE *);
/* getnameinfo: provided by the wasip2 socket graft at link, but its decl is
   guarded out of <netdb.h> (the NI_* flag macros are not). */
#include <sys/socket.h>
int getnameinfo(const struct sockaddr *, socklen_t, char *, socklen_t,
                char *, socklen_t, int);
#ifdef __cplusplus
}
#endif
#endif
#endif /* __wasi__ */
#endif /* PG_WASI_SHIM_H */
