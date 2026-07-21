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

    # Wrap the script: redirect output to the actuals file, run the
    # script, restore output. The CLI does not have a batch-mode flag,
    # so we build a driver script that uses `.read` + `.output` and
    # exits cleanly.
    driver="$(mktemp -t ducklink-conformance.XXXXXX)"
    trap 'rm -f "$driver"' EXIT
    cat > "$driver" <<EOF
.output $actual
.read $script
.output
.exit
EOF

    # Every ducklink test starts from a fresh in-memory DB. The CLI's
    # default is :memory: too, so no path is needed.
    if ! "$DUCKLINK_CLI" -- -init "$driver" 2>/dev/null; then
        echo "FAIL $name — CLI exited non-zero"
        FAILED=$((FAILED + 1))
        continue
    fi

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
