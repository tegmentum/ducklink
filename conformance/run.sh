#!/usr/bin/env bash
# Ducklink conformance runner for the workspace host.
#
# Drives every `scripts/NN-*.sql` through the ducklink CLI's `.read`
# + `.output` machinery, captures the output into `actuals/NN-*.out`,
# and diffs against `expected/NN-*.out`. Any mismatch exits non-zero
# with the diff shown.
#
# The suite itself is host-agnostic — see `conformance/README.md`.
# This driver is workspace-specific because it depends on the
# workspace CLI's dot-command surface.
#
# Usage:
#   ./run.sh                         # run every script in scripts/
#   ./run.sh scripts/01-*.sql        # run a subset
#   DUCKLINK_CLI=/path/to/binary ./run.sh
#   ./run.sh --bless                 # rewrite expected/ from actuals
#                                    # (use only after a deliberate
#                                    #  surface change; commit with a
#                                    #  matching STABILITY / CHANGELOG
#                                    #  entry in ducklink-extension)

set -euo pipefail

here() { cd "$(dirname "$0")" && pwd; }
ROOT="$(here)"
SCRIPTS_DIR="$ROOT/scripts"
EXPECTED_DIR="$ROOT/expected"
ACTUALS_DIR="$ROOT/actuals"

DUCKLINK_CLI="${DUCKLINK_CLI:-$ROOT/../target/release/ducklink}"

BLESS=0
FILTER=()
for arg in "$@"; do
    case "$arg" in
        --bless) BLESS=1 ;;
        -h|--help)
            grep -E '^# ' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) FILTER+=("$arg") ;;
    esac
done

if [ ! -x "$DUCKLINK_CLI" ]; then
    echo "ducklink CLI not found or not executable at: $DUCKLINK_CLI"
    echo "build with \`cargo build --release -p ducklink-cli\` or set \$DUCKLINK_CLI"
    exit 2
fi

mkdir -p "$ACTUALS_DIR"

if [ ${#FILTER[@]} -eq 0 ]; then
    mapfile -t SCRIPTS < <(find "$SCRIPTS_DIR" -maxdepth 1 -name "*.sql" | sort)
else
    SCRIPTS=("${FILTER[@]}")
fi

FAILED=0
PASSED=0

for script in "${SCRIPTS[@]}"; do
    name="$(basename "$script" .sql)"
    actual="$ACTUALS_DIR/$name.out"
    expected="$EXPECTED_DIR/$name.out"

    # Preprocess: strip `LOAD ducklink;` lines. The scripts include it
    # for portability across hosts (some hosts load ducklink as a real
    # extension), but this workspace's DuckDB is compiled without
    # external extension loading — the ducklink surface is expected to
    # be BAKED IN by ducklink-host, not loaded. This mirrors what the
    # extension repo's Rust runner does.
    #
    # Everything the wasm CLI touches has to live under a WASI preopen.
    # Keep the preprocessed script + actuals under $ROOT (which we
    # preopen as `/conf`) so a single preopen mapping covers all I/O.
    tmp_dir="$ROOT/.tmp"
    mkdir -p "$tmp_dir"
    preprocessed="$tmp_dir/$name.sql"
    grep -viE '^\s*LOAD\s+ducklink\s*;?\s*$' "$script" > "$preprocessed"

    # Build a driver. Paths are the GUEST-visible mapping of `--dir
    # $ROOT::/conf`, so a `.read /conf/.tmp/NN.sql` inside the wasm
    # sandbox resolves to `$ROOT/.tmp/NN.sql` on the host.
    driver_sql=".output /conf/actuals/$name.out
.read /conf/.tmp/$name.sql
.output
.exit"

    if ! "$DUCKLINK_CLI" \
            --dir "$ROOT::/conf" \
            -- -c "$driver_sql" \
            > /dev/null 2>&1; then
        rm -f "$preprocessed"
        echo "FAIL $name — CLI exited non-zero"
        FAILED=$((FAILED + 1))
        continue
    fi
    rm -f "$preprocessed"

    if [ "$BLESS" -eq 1 ]; then
        mkdir -p "$EXPECTED_DIR"
        cp "$actual" "$expected"
        echo "BLESS $name"
        continue
    fi

    if ! diff -u "$expected" "$actual"; then
        echo "FAIL $name"
        FAILED=$((FAILED + 1))
    else
        echo "PASS $name"
        PASSED=$((PASSED + 1))
    fi
done

echo
if [ "$FAILED" -eq 0 ]; then
    echo "conformance: $PASSED passed"
    exit 0
else
    echo "conformance: $PASSED passed, $FAILED failed"
    exit 1
fi
