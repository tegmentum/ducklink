#pragma once

#ifdef __wasi__

// postgres_scanner needs the REAL wasi getaddrinfo (libpq resolves hosts over
// wasi:sockets; numeric IPs are handled by the getaddrinfo wrapper). When
// PG_WASI_REAL_NETDB is set (the postgres compile, via cmake/postgres-deps.cmake)
// defer to the real <netdb.h>, not the no-socket stub below which exists so core
// DuckDB compiles without sockets.
#if defined(PG_WASI_REAL_NETDB)
#include_next <netdb.h>
#else

#include <stdint.h>
#include <sys/socket.h>

struct hostent {
    char *h_name;
    char **h_aliases;
    int h_addrtype;
    int h_length;
    char **h_addr_list;
};

#define h_addr h_addr_list[0]

struct addrinfo {
    int ai_flags;
    int ai_family;
    int ai_socktype;
    int ai_protocol;
    socklen_t ai_addrlen;
    struct sockaddr *ai_addr;
    char *ai_canonname;
    struct addrinfo *ai_next;
};

static inline void freeaddrinfo(struct addrinfo *res) {
    (void)res;
}

static inline int getaddrinfo(const char *, const char *, const struct addrinfo *, struct addrinfo **) {
    return -1;
}

static inline const char *gai_strerror(int) {
    return "getaddrinfo unsupported";
}

#endif // PG_WASI_REAL_NETDB
#endif // __wasi__
