// Subprocess libc stubs for wasm32-wasip2 (wasi-libc has no posix_spawn / waitpid
// / kill / pipe). They let the Azure SDK's AzureCliCredential link; each fails
// with ENOSYS at runtime, so that one credential is unavailable while the env /
// managed-identity / client-secret credentials work normally.
#include <errno.h>
#include <sys/types.h>

#include "spawn.h"
#include "sys/wait.h"

int posix_spawn_file_actions_init(posix_spawn_file_actions_t *a) {
  (void)a;
  return 0;
}
int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *a) {
  (void)a;
  return 0;
}
int posix_spawn_file_actions_addclose(posix_spawn_file_actions_t *a, int fd) {
  (void)a;
  (void)fd;
  return 0;
}
int posix_spawn_file_actions_adddup2(posix_spawn_file_actions_t *a, int f, int g) {
  (void)a;
  (void)f;
  (void)g;
  return 0;
}
int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                const posix_spawnattr_t *at, char *const argv[], char *const envp[]) {
  (void)pid;
  (void)path;
  (void)fa;
  (void)at;
  (void)argv;
  (void)envp;
  errno = ENOSYS;
  return ENOSYS;
}
int posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *fa,
                 const posix_spawnattr_t *at, char *const argv[], char *const envp[]) {
  return posix_spawn(pid, file, fa, at, argv, envp);
}
pid_t waitpid(pid_t pid, int *status, int options) {
  (void)pid;
  (void)status;
  (void)options;
  errno = ENOSYS;
  return -1;
}
int kill(pid_t pid, int sig) {
  (void)pid;
  (void)sig;
  errno = ENOSYS;
  return -1;
}
int pipe(int fds[2]) {
  (void)fds;
  errno = ENOSYS;
  return -1;
}
