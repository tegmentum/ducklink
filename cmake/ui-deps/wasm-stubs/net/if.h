#pragma once
// <net/if.h> stub for wasm. httplib includes this right before using getnameinfo;
// on wasm the real <netdb.h> isn't always in scope (header-ordering with
// duckdb.hpp), so declare getnameinfo + its NI_* flags here. The listen path is
// bypassed, so these are compile-only (never called).
#include <sys/socket.h>
#ifdef __cplusplus
extern "C" {
#endif
unsigned int if_nametoindex(const char *);
char *if_indextoname(unsigned int, char *);
int getnameinfo(const struct sockaddr *, socklen_t, char *, socklen_t, char *,
                socklen_t, int);
#ifdef __cplusplus
}
#endif
#define IF_NAMESIZE 16
#ifndef NI_MAXHOST
#define NI_MAXHOST 1025
#endif
#ifndef NI_NUMERICHOST
#define NI_NUMERICHOST 1
#endif
