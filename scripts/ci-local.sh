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

# --list short-circuits to job listing (validates the workflow parses).
for arg in "$@"; do
  if [[ "$arg" == "--list" || "$arg" == "-l" ]]; then
    exec act -W .github/workflows/smoke-tests.yml -l
  fi
done

# Default GitHub event for a workflow is `push`. Pass through any extra flags.
exec act push -W .github/workflows/smoke-tests.yml "$@"
