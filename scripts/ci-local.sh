#!/usr/bin/env bash
# Run the smoke-tests GitHub Actions workflow locally with nektos/act, in Docker.
# A stopgap for CI until the repo is public (free Actions) or billing is enabled.
#
# Usage:
#   scripts/ci-local.sh                 # run the smoke-tests workflow
#   scripts/ci-local.sh -v              # verbose
#   scripts/ci-local.sh --list          # list jobs without running
#
# The first run is slow: it pulls the runner image, compiles cargo-component /
# wac / wasmtime, and builds the patched DuckDB wasm archive. Subsequent runs
# reuse the cached archive (actions/cache) and the --reuse container.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! command -v act >/dev/null 2>&1; then
  echo "act is not installed. Install it with: brew install act" >&2
  echo "(see https://github.com/nektos/act)" >&2
  exit 1
fi

if ! docker info >/dev/null 2>&1; then
  echo "Docker does not appear to be running. Start Docker and retry." >&2
  exit 1
fi

# act connects to /var/run/docker.sock by default, which may symlink to a
# stopped engine (e.g. Docker Desktop) even when `docker` itself uses a
# different active context (colima, etc.). Point act at the active context's
# endpoint so it talks to the same daemon the docker CLI does.
if [[ -z "${DOCKER_HOST:-}" ]]; then
  ctx_sock="$(docker context inspect -f '{{.Endpoints.docker.Host}}' 2>/dev/null || true)"
  if [[ -n "$ctx_sock" ]]; then
    export DOCKER_HOST="$ctx_sock"
  fi
fi

# --list short-circuits to job listing (validates the workflow parses).
for arg in "$@"; do
  if [[ "$arg" == "--list" || "$arg" == "-l" ]]; then
    exec act -W .github/workflows/smoke-tests.yml -l
  fi
done

# act bind-mounts a Docker socket into the runner container. The smoke workflow
# never uses docker-in-docker, and some engines (colima) cannot bind-mount a unix
# socket file ("operation not supported" — a Lima VM limitation); act 0.2.89 also
# ignores `--container-daemon-socket -` when DOCKER_HOST is set, falling back to
# mounting it anyway. Point act at a dummy *regular file* under $HOME (mountable
# on colima and Docker Desktop alike, and never actually used) to sidestep both.
noop_sock="$HOME/.cache/act/noop-docker.sock"
mkdir -p "$(dirname "$noop_sock")"
: > "$noop_sock"

# Default GitHub event for a workflow is `push`. Pass through any extra flags.
exec act push -W .github/workflows/smoke-tests.yml \
  --container-daemon-socket="$noop_sock" "$@"
