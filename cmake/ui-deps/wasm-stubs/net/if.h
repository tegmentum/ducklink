#pragma once
// <net/if.h> stub for wasm (httplib references it; the listen path is bypassed).
unsigned int if_nametoindex(const char *);
char *if_indextoname(unsigned int, char *);
#define IF_NAMESIZE 16
