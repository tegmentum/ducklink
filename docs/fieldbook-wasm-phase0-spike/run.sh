#!/usr/bin/env bash
# Fieldbook browser Phase 0 spike — serve spike.html + fieldbook.wasm on a
# tiny local HTTP server. duckdb-wasm can't be loaded via file:// (worker
# instantiation needs http/https), so a static server is required.
#
# Usage:
#   bash docs/fieldbook-wasm-phase0-spike/run.sh              # port 8788
#   PORT=9000 bash docs/fieldbook-wasm-phase0-spike/run.sh    # override
#
# Prints the URL, then blocks. Ctrl-C to stop.
set -euo pipefail

PORT="${PORT:-8788}"
SPIKE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SPIKE_DIR/../.." && pwd)"

# Stage fieldbook.wasm next to spike.html so the fetch("./fieldbook.wasm")
# in the page resolves. We use whichever build the repo already produced
# (wasip1 is the current target for extensions per the mosaic Phase 1 model).
FB_WASM=""
for cand in \
  "$REPO_ROOT/target/wasm32-wasip1/release/fieldbook.wasm" \
  "$REPO_ROOT/target/wasm32-wasip2/release/fieldbook.wasm" \
  "$REPO_ROOT/artifacts/extensions/fieldbook.wasm"; do
  if [[ -f "$cand" ]]; then FB_WASM="$cand"; break; fi
done

if [[ -z "$FB_WASM" ]]; then
  echo "warn: no fieldbook.wasm found; the extension-load section of the" >&2
  echo "      spike will 404 rather than reach the expected wasm-shape rejection." >&2
else
  cp -f "$FB_WASM" "$SPIKE_DIR/fieldbook.wasm"
  echo ">>> staged $(basename "$FB_WASM") ($(wc -c <"$FB_WASM" | tr -d ' ') bytes)"
fi

cat <<EOF

>>> serving spike on http://127.0.0.1:$PORT/spike.html
>>>   (Ctrl-C to stop)

The page loads @duckdb/duckdb-wasm from jsdelivr, runs a hard-coded
query, then attempts to load ./fieldbook.wasm to demonstrate that the
two stacks are incompatible. Check the browser devtools console for
each stage's status; the page itself prints inline.

EOF

cd "$SPIKE_DIR"
exec python3 -m http.server "$PORT" --bind 127.0.0.1
