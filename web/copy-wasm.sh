#!/usr/bin/env bash
# Stage the built `ducklink_core.wasm` + demo-extension wasms into `web/public/`.
#
# Works from a normal checkout (paths relative to the repo root) *and* from a
# git worktree under `.claude/worktrees/<name>/web` (where `../..` no longer
# lands at `~/git`). Resolves the repo root via `git rev-parse --show-toplevel`
# once, then walks from there rather than assuming a fixed relative depth.
#
# Overrides:
#   DUCKLINK_CORE_WASM     — absolute path to ducklink_core.wasm
#   EXTENSION_ARTIFACT_DIR — directory holding sample_extension.wasm etc.
#
# Missing extension wasms are warned-about, not fatal (the plain-query smoke
# doesn't need them).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
# When invoked from `.claude/worktrees/<name>/web`, GIT_COMMON_DIR points back
# at the parent repo's `.git` — its parent is the main repo checkout, which is
# where `../duckdb-wasm` and `artifacts/` actually live.
COMMON_GIT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --git-common-dir)"
[[ "$COMMON_GIT_DIR" != /* ]] && COMMON_GIT_DIR="$SCRIPT_DIR/$COMMON_GIT_DIR"
MAIN_REPO_ROOT="$(cd "$(dirname "$COMMON_GIT_DIR")" && pwd)"

# ducklink_core.wasm: prefer an explicit override; otherwise look next to the
# main repo checkout for a `../duckdb-wasm/target/...` build (the current
# canonical layout after the duckdb-wasm split). Leave the existing
# public/ducklink_core.wasm in place if no source is found so re-runs on the
# same worktree don't wipe a manually-staged copy.
CORE_DEFAULT="$MAIN_REPO_ROOT/../duckdb-wasm/target/wasm32-wasip2/release/ducklink_core.wasm"
CORE_WASM="${DUCKLINK_CORE_WASM:-$CORE_DEFAULT}"
mkdir -p "$SCRIPT_DIR/public"
if [[ -f "$CORE_WASM" ]]; then
    cp "$CORE_WASM" "$SCRIPT_DIR/public/ducklink_core.wasm"
elif [[ -f "$SCRIPT_DIR/public/ducklink_core.wasm" ]]; then
    echo "copy-wasm: reusing existing web/public/ducklink_core.wasm (source $CORE_WASM missing)" >&2
else
    echo "error: ducklink_core.wasm not found at $CORE_WASM and none staged." >&2
    exit 1
fi

# Extension artifacts: main-repo `artifacts/extensions/` is the canonical
# location; the worktree checkout does not stage them. Missing wasms warn and
# skip so the plain-query smoke (`verify` / `verify-prepared` / `verify-tvm-
# spill`) still works without the extension set.
EXT_DIR="${EXTENSION_ARTIFACT_DIR:-$MAIN_REPO_ROOT/artifacts/extensions}"
for name in sample_extension cron cron_scheduler aba; do
    src="$EXT_DIR/$name.wasm"
    if [[ -s "$src" ]]; then
        cp "$src" "$SCRIPT_DIR/public/$name.wasm"
    else
        echo "copy-wasm: skipping $name.wasm (missing or empty: $src)" >&2
    fi
done
