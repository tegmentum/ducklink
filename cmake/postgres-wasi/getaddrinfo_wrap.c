/* getaddrinfo wrapper for wasm: wasi-libc's getaddrinfo always routes through
   wasi:sockets ip-name-lookup resolve-addresses, which wasmtime rejects for
   numeric IP literals + loopback ("getaddrinfo unsupported") even with
   AI_NUMERICHOST -- so host=127.0.0.1 / hostaddr=... never connect. Real
   hostnames resolve fine (curl/httpfs prove it). This wrapper parses numeric
   IPv4/IPv6 literals locally (no resolve-addresses) and delegates everything
   else to the real getaddrinfo. Reached via Rust trampolines __wrap_getaddrinfo/__wrap_freeaddrinfo in
   the root crate (lib.rs), which --wrap=getaddrinfo redirects to (build.rs). Benefits any wasm client
   connecting by numeric IP; libpq (postgres_scanner) is the first user. */
#include <netdb.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <stdlib.h>
#include <string.h>

int __real_getaddrinfo(const char *, const char *, const struct addrinfo *,
                       struct addrinfo **);
void __real_freeaddrinfo(struct addrinfo *);

/* tag in ai_flags so freeaddrinfo knows we built this node */
#define PG_AI_NUMERIC_TAG 0x40000000

/* manual dotted-quad IPv4 parse (no inet_pton dependency) -> 4 bytes, or 0 */
static int pg_parse_ipv4(const char *s, unsigned char out[4]) {
    if (!s) return 0;
    const char *p = s;
    for (int i = 0; i < 4; i++) {
        if (*p < '0' || *p > '9') return 0;
        int v = 0, d = 0;
        while (*p >= '0' && *p <= '9') { v = v * 10 + (*p - '0'); p++; if (++d > 3 || v > 255) return 0; }
        out[i] = (unsigned char)v;
        if (i < 3) { if (*p != '.') return 0; p++; }
    }
    return *p == '\0';
}

int pg_wasi_getaddrinfo(const char *node, const char *service,
                       const struct addrinfo *hints, struct addrinfo **res) {
    unsigned char ip4[4];
    struct in6_addr a6;
    int is4 = pg_parse_ipv4(node, ip4);
    int is6 = !is4 && node && inet_pton(AF_INET6, node, &a6) == 1;
    if (!is4 && !is6)
        return __real_getaddrinfo(node, service, hints, res);

    int port = service ? atoi(service) : 0;
    struct addrinfo *ai = calloc(1, sizeof(*ai));
    if (!ai)
        return EAI_MEMORY;
    if (is4) {
        struct sockaddr_in *sin = calloc(1, sizeof(*sin));
        if (!sin) { free(ai); return EAI_MEMORY; }
        sin->sin_family = AF_INET;
        memcpy(&sin->sin_addr, ip4, 4);
        sin->sin_port = htons((unsigned short)port);
        ai->ai_family = AF_INET;
        ai->ai_addrlen = sizeof(*sin);
        ai->ai_addr = (struct sockaddr *)sin;
    } else {
        struct sockaddr_in6 *sin6 = calloc(1, sizeof(*sin6));
        if (!sin6) { free(ai); return EAI_MEMORY; }
        sin6->sin6_family = AF_INET6;
        sin6->sin6_addr = a6;
        sin6->sin6_port = htons((unsigned short)port);
        ai->ai_family = AF_INET6;
        ai->ai_addrlen = sizeof(*sin6);
        ai->ai_addr = (struct sockaddr *)sin6;
    }
    ai->ai_socktype = (hints && hints->ai_socktype) ? hints->ai_socktype : SOCK_STREAM;
    ai->ai_protocol = hints ? hints->ai_protocol : 0;
    ai->ai_flags = PG_AI_NUMERIC_TAG;
    ai->ai_next = NULL;
    *res = ai;
    return 0;
}

void pg_wasi_freeaddrinfo(struct addrinfo *ai) {
    if (ai && (ai->ai_flags & PG_AI_NUMERIC_TAG)) {
        while (ai) {
            struct addrinfo *next = ai->ai_next;
            free(ai->ai_addr);
            free(ai);
            ai = next;
        }
        return;
    }
    __real_freeaddrinfo(ai);
}
