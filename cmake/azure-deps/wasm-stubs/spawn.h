// Minimal <spawn.h> for wasm32-wasip2 (wasi-libc has no subprocess support).
// Lets the Azure SDK's AzureCliCredential compile + link; posix_spawn fails at
// runtime (ENOSYS), so that credential is unavailable -- env / managed-identity
// / client-secret credentials still work. See cmake/azure-deps/README.md.
#pragma once
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
  int _unused;
} posix_spawn_file_actions_t;

typedef struct {
  int _unused;
} posix_spawnattr_t;

int posix_spawn_file_actions_init(posix_spawn_file_actions_t *);
int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *);
int posix_spawn_file_actions_addclose(posix_spawn_file_actions_t *, int);
int posix_spawn_file_actions_adddup2(posix_spawn_file_actions_t *, int, int);
int posix_spawn(pid_t *, const char *, const posix_spawn_file_actions_t *,
                const posix_spawnattr_t *, char *const[], char *const[]);
int posix_spawnp(pid_t *, const char *, const posix_spawn_file_actions_t *,
                 const posix_spawnattr_t *, char *const[], char *const[]);

// wasi's <unistd.h>/<signal.h> don't declare these; the cli credential needs them
// (stubbed to ENOSYS in azure_subprocess_stubs.c).
int pipe(int[2]);
int kill(pid_t, int);

#ifdef __cplusplus
}
#endif
