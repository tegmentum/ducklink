// Minimal <sys/wait.h> for wasm32-wasip2 (no process management). Pairs with the
// spawn.h stub for the Azure SDK's AzureCliCredential.
#pragma once
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WNOHANG 1
#define WIFEXITED(s) (((s) & 0x7f) == 0)
#define WEXITSTATUS(s) (((s) >> 8) & 0xff)
#define WIFSIGNALED(s) (((s) & 0x7f) != 0 && ((s) & 0x7f) != 0x7f)
#define WTERMSIG(s) ((s) & 0x7f)

pid_t waitpid(pid_t, int *, int);

#ifdef __cplusplus
}
#endif
