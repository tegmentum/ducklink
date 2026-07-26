#!/usr/bin/env bash
# Mosaic Phase 0 spike — end-to-end transport smoke.
#
# Proves: browser -> ducklink serve -> sql-kind route -> DuckDB -> JSON -> browser.
# No mosaic.wasm, no mosaic-core, no Arrow. Just the transport path.
#
# Usage:
#   bash docs/mosaic-phase0-spike/run.sh                 # port 8787
#   PORT=9000 bash docs/mosaic-phase0-spike/run.sh       # override port
#
# The script exits when the server dies (Ctrl-C to stop). It intentionally
# blocks in the foreground so the operator sees the request log.

set -euo pipefail

PORT="${PORT:-8787}"
SPIKE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SPIKE_DIR/../.." && pwd)"
WORK_DIR="$(mktemp -d -t ducklink-p0-XXXXXX)"
DB_FILE="spike.duckdb"          # relative — cwd is ducklink's fs preopen

DUCKLINK_BIN="${DUCKLINK_BIN:-$REPO_ROOT/target/release/ducklink}"
if [[ ! -x "$DUCKLINK_BIN" ]]; then
  echo "ducklink not built at $DUCKLINK_BIN" >&2
  echo "run: (cd $REPO_ROOT && cargo build --release -p ducklink-host --bin ducklink)" >&2
  exit 1
fi

INDEX_HTML="$(cat "$SPIKE_DIR/index.html")"

echo ">>> spike workdir: $WORK_DIR"
cd "$WORK_DIR"

# 1) Start ducklink with a fresh disk-backed DB.
echo ">>> starting ducklink serve on :$PORT"
"$DUCKLINK_BIN" serve --db "$DB_FILE" --port "$PORT" --init-routes 2>server.log &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true; wait 2>/dev/null || true; echo ">>> stopped"; echo ">>> logs at $WORK_DIR/server.log"' EXIT

# Wait for /health.
for _ in $(seq 1 40); do
  if curl -sf -m 1 "http://127.0.0.1:$PORT/health" >/dev/null; then break; fi
  sleep 0.1
done
if ! curl -sf -m 1 "http://127.0.0.1:$PORT/health" >/dev/null; then
  echo "!!! ducklink did not come up — see $WORK_DIR/server.log" >&2
  tail -20 server.log >&2 || true
  exit 1
fi
echo ">>> up"

# tiny helper: POST SQL to /sql, error on non-2xx
post_sql() {
  local sql="$1"
  local out
  out="$(curl -sf -X POST "http://127.0.0.1:$PORT/sql" --data-binary "$sql")" || {
    echo "!!! SQL failed: $sql" >&2
    exit 1
  }
  echo "$out"
}

# 2) Seed a fixture: 20 rows of synthetic daily time series.
echo ">>> seeding fixture table"
post_sql "CREATE TABLE fixture AS
          SELECT (DATE '2026-01-01' + INTERVAL (i) DAY) AS ts,
                 (30 + 10*sin(i*0.5))::DOUBLE          AS value
          FROM range(20) t(i)" >/dev/null

# 3) Register the two routes we need:
#    - GET /            static-kind, serves index.html verbatim
#    - POST /api/query  sql-kind, runs $body via query(...) and returns JSON

# Escape the HTML for the SQL string literal (single quotes -> '').
ESCAPED_HTML="$(printf '%s' "$INDEX_HTML" | sed "s/'/''/g")"

echo ">>> registering /"
post_sql "INSERT INTO routes (method, pattern, handler, kind, ctype, priority)
          VALUES ('GET', '/', '$ESCAPED_HTML', 'static', 'text/html; charset=utf-8', 10)" >/dev/null

echo ">>> registering POST /api/query"
post_sql "INSERT INTO routes (method, pattern, handler, kind, ctype, priority)
          VALUES ('POST', '/api/query',
                  'SELECT * FROM query(\$body)',
                  'sql', 'application/json', 10)" >/dev/null

# 4) Sanity: hit the sql route from the shell before the browser does.
echo ">>> sanity: POST /api/query"
curl -s -X POST "http://127.0.0.1:$PORT/api/query" \
     --data-binary "SELECT * FROM fixture ORDER BY ts LIMIT 3"
echo

cat <<EOF

>>> open in a browser:
    http://127.0.0.1:$PORT/

    You should see a small line-and-dot chart of the sin-wave fixture,
    followed by a table of the 20 rows. Both come from a single
    /api/query round-trip.

>>> logs: tail -f $WORK_DIR/server.log
>>> Ctrl-C to stop.
EOF

wait "$SERVER_PID"
