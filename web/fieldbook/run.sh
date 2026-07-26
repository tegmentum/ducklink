#!/usr/bin/env bash
# Serve the fieldbook browser demo (Product 2 of the Fieldbook-wasm
# initiative). A tiny local HTTP server is required — the ducklink core
# component is loaded via jco which needs the wasm bytes fetched over
# http(s) (file:// URLs can't be used with WebAssembly.instantiateStreaming
# / JSPI).
#
# Usage:
#   bash web/fieldbook/run.sh                # default port 8789
#   PORT=9000 bash web/fieldbook/run.sh      # override
#
# Prereqs: `make fieldbook-browser` (from the repo root) must have populated
# web/fieldbook/dist/{index.html,assets/,ducklink_core.wasm,fieldbook.wasm}.
# The script errors out early if any of those is missing.
set -euo pipefail

PORT="${PORT:-8789}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST="$HERE/dist"

for f in index.html assets ducklink_core.wasm fieldbook.wasm; do
  if [[ ! -e "$DIST/$f" ]]; then
    echo "error: $DIST/$f not found." >&2
    echo "       run 'make fieldbook-browser' from the ducklink repo root first." >&2
    exit 1
  fi
done

CORE_BYTES=$(wc -c < "$DIST/ducklink_core.wasm" | tr -d ' ')
FB_BYTES=$(wc -c < "$DIST/fieldbook.wasm" | tr -d ' ')
JS_BYTES=$(du -sb "$DIST/assets" 2>/dev/null | awk '{print $1}' \
           || find "$DIST/assets" -type f -exec cat {} + | wc -c | tr -d ' ')

cat <<EOF

>>> serving fieldbook demo on http://127.0.0.1:$PORT/
>>>   dist/assets/*            $JS_BYTES bytes total (bundle + jco transpiler wasm)
>>>   dist/ducklink_core.wasm  $CORE_BYTES bytes
>>>   dist/fieldbook.wasm      $FB_BYTES bytes
>>>   (Ctrl-C to stop)

Open the URL in Chrome 137+ (JSPI required — see db.js comments).
The page:
  1. loads the WIT-based ducklink DuckDB core,
  2. opens a memfs-backed database at /fieldbook.duckdb,
  3. bootstraps the __fieldbook_* schema,
  4. seeds two starter cells,
  5. lets you run/add/delete cells and download the .duckdb file.

EOF

cd "$DIST"
exec python3 -m http.server "$PORT" --bind 127.0.0.1
