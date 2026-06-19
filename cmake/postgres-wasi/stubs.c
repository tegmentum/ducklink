/* No-op implementations of the POSIX signal API libpq references for SIGPIPE
   handling (wasi has no signals) + getpwuid (no local users). Weak so any real
   wasi-libc impl wins. */
#include <stddef.h>
#include <errno.h>
#include "pwd.h"
typedef unsigned long __sigset_t_compat;
#define WEAK __attribute__((weak))
struct sigaction;
WEAK int sigaction(int s, const struct sigaction *a, struct sigaction *o) { (void)s;(void)a;(void)o; return 0; }
WEAK int sigemptyset(void *s) { (void)s; return 0; }
WEAK int sigfillset(void *s) { (void)s; return 0; }
WEAK int sigaddset(void *s, int n) { (void)s;(void)n; return 0; }
WEAK int sigdelset(void *s, int n) { (void)s;(void)n; return 0; }
WEAK int sigismember(const void *s, int n) { (void)s;(void)n; return 0; }
WEAK int sigpending(void *s) { (void)s; return 0; }
WEAK int sigprocmask(int h, const void *s, void *o) { (void)h;(void)s;(void)o; return 0; }
WEAK int pthread_sigmask(int h, const void *s, void *o) { (void)h;(void)s;(void)o; return 0; }
WEAK int sigwait(const void *s, int *n) { (void)s; if(n)*n=0; return 0; }
WEAK struct passwd *getpwuid(uid_t u) { (void)u; return NULL; }
WEAK int getpwuid_r(uid_t u, struct passwd *p, char *b, size_t n, struct passwd **r) {
    (void)u;(void)p;(void)b;(void)n; if(r)*r=NULL; return 0; }
WEAK uid_t getuid(void) { return 0; }
WEAK uid_t geteuid(void) { return 0; }
/* unix-socket peer creds; never reached for TCP connections */
WEAK int getpeereid(int fd, uid_t *u, gid_t *g) { (void)fd; if(u)*u=0; if(g)*g=0; return -1; }
WEAK void *popen(const char *c, const char *m) { (void)c;(void)m; return NULL; }
WEAK int pclose(void *f) { (void)f; return -1; }
WEAK const char *gai_strerror(int code) {
    switch (code) {
    case 0: return "Success";
    case -1: return "Temporary failure in name resolution";
    case -2: return "Name or service not known";
    case -3: return "Bad value for ai_flags";
    case -8: return "Servname not supported for ai_socktype";
    default: return "name resolution error";
    }
}
