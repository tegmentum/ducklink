#!/usr/bin/env bash
# Capture the DuckDB UI SPA (ui.duckdb.org) for OFFLINE serving by `ducklink ui`.
# The SPA is a single monolithic bundle (no lazy chunks): index.html + a hashed
# bundle.js + css, plus a function-docs file used for autocomplete. The host serves
# these for /<asset> in offline mode (or proxies ui.duckdb.org in online mode), and
# bridges /ddb/* to the wasm DuckDB core.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/web/duckdb-ui}"
BASE="${DUCKDB_UI_URL:-https://ui.duckdb.org}"
UA="duckdb"

mkdir -p "$OUT/assets/docs/v3"
echo ">> index.html"
curl -fsS -m 60 -A "$UA" "$BASE/" -o "$OUT/index.html"

bundle=$(grep -oE 'hatchling\.[0-9a-f]+\.bundle\.js' "$OUT/index.html" | head -1)
css=$(grep -oE 'hatchling\.[0-9a-f]+\.css' "$OUT/index.html" | head -1)
[[ -n "$bundle" && -n "$css" ]] || { echo "could not find bundle/css in index.html" >&2; exit 1; }

echo ">> $bundle ($(curl -fsS -m 180 -A "$UA" "$BASE/$bundle" -o "$OUT/$bundle" -w '%{size_download}' ) bytes)"
echo ">> $css ($(curl -fsS -m 60 -A "$UA" "$BASE/$css" -o "$OUT/$css" -w '%{size_download}') bytes)"

# Autocomplete function docs (optional).
if curl -fsS -m 60 -A "$UA" "$BASE/assets/docs/v3/function_docs.jsonl" \
     -o "$OUT/assets/docs/v3/function_docs.jsonl" 2>/dev/null; then
  echo ">> assets/docs/v3/function_docs.jsonl"
else
  echo ">> (function_docs.jsonl unavailable, skipped)"
fi

# Record which bundle/css this capture used (the host rewrites index references if needed).
echo ">> captured to $OUT ($(du -sh "$OUT" | cut -f1))"
