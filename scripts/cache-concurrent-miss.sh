#!/usr/bin/env bash
# cache-concurrent-miss.sh -- end-to-end proof that ducklink:cache serialises
# concurrent misses on the SAME source URI via the wasm-side file-lock host
# import (duckdb:extension/file-lock@5.0.0, added in commit e84d8f9).
#
# Motivating scenario from the design memo (extensions/cache-component/src/
# lib.rs module doc, "Locking (v0.2)"):
#
#   N processes race calling cache('http://.../big-file') on a URL not
#   yet cached. The winner performs a single HTTP GET, publishes the
#   blob + catalog row, then the losers observe the winner's entry via a
#   re-lookup under lock and return the SAME file:// URI without ever
#   touching the network.
#
# What this script asserts:
#   1. The tiny HTTP server sees EXACTLY 1 GET for /payload (not N).
#   2. All N ducklink invocations exit 0.
#   3. All N stdouts include the same file:// URI (the published blob).
#   4. The blob at that path hashes to the expected sha256 of the body.
#   5. The catalog (__cache_entries via <root>/metadata.db) has exactly
#      one row for the URL.
#
# Without the wasm file-lock wiring the request count assertion trips —
# each racing process would fire its own GET.
#
# Prerequisites (fail fast with a clear message if any are missing):
#   * target/release/ducklink       (make host)
#   * artifacts/extensions/cache.wasm composed AFTER commit e84d8f9 so
#     the guest world imports duckdb:extension/file-lock (make cache)
#   * python3 available on PATH (bundled with macOS + most Linux)
#   * sha256sum OR shasum available
#
# Run:
#   bash scripts/cache-concurrent-miss.sh
# Or, override the worker count (default 6):
#   N=8 bash scripts/cache-concurrent-miss.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${BIN:-$ROOT/target/release/ducklink}"
CACHE_WASM="${CACHE_WASM:-$ROOT/artifacts/extensions/cache.wasm}"
N="${N:-6}"
BODY_TEXT="${BODY_TEXT:-ducklink-cache-concurrent-miss-fixed-body-42}"

fail() {
  echo "cache-concurrent-miss: FAIL: $*" >&2
  exit 1
}

info() {
  echo "cache-concurrent-miss: $*" >&2
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

[[ -x "$BIN" ]] || fail "ducklink binary missing at $BIN (run: make host)"
[[ -f "$CACHE_WASM" ]] || fail "cache.wasm missing at $CACHE_WASM (run: make cache)"
command -v python3 >/dev/null 2>&1 || fail "python3 not on PATH"

# Verify the composed cache.wasm actually imports file-lock. Without it, the
# whole point of the test is void (the resolver silently falls through the
# advisory-lock path). Requires wasm-tools; if unavailable, warn and continue.
if command -v wasm-tools >/dev/null 2>&1; then
  if ! wasm-tools component wit "$CACHE_WASM" 2>/dev/null | grep -q "file-lock"; then
    fail "cache.wasm at $CACHE_WASM does NOT import duckdb:extension/file-lock. \
This is the pre-e84d8f9 build; rebuild via 'make cache'."
  fi
  info "verified cache.wasm imports duckdb:extension/file-lock"
else
  info "wasm-tools missing; skipping file-lock world check (test result still valid)"
fi

SHA256_CMD=""
if command -v sha256sum >/dev/null 2>&1; then
  SHA256_CMD="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA256_CMD="shasum -a 256"
else
  fail "no sha256sum / shasum on PATH"
fi

EXPECTED_SHA=$(printf '%s' "$BODY_TEXT" | $SHA256_CMD | awk '{print $1}')

# ---------------------------------------------------------------------------
# Workspace
# ---------------------------------------------------------------------------

# Put the workdir under $ROOT/target/ so it's covered by the cwd preopen
# ducklink installs (host=cwd, guest="."). WASI's fs shim resolves paths
# only under a preopen, so a /tmp workdir would be silently unwritable
# from the guest — mirrors the note in crates/ducklink-host/tests/
# cron_wasm_driver.rs.
mkdir -p "$ROOT/target"
WORKDIR="$(mktemp -d "$ROOT/target/ducklink-cache-concurrent.XXXXXX")"
# Cache root as an absolute path (cache-component's cache_root() reads
# DUCKLINK_LOCAL_CACHE verbatim). Because WORKDIR is under $ROOT and we cd
# to $ROOT for each worker, the guest can open this absolute path via the
# cwd preopen.
CACHE_ROOT="$WORKDIR/cache"
LOG_DIR="$WORKDIR/logs"
HIT_LOG="$WORKDIR/hits.log"
BODY_FILE="$WORKDIR/body.txt"
SERVER_LOG="$WORKDIR/server.log"
mkdir -p "$CACHE_ROOT" "$CACHE_ROOT/objects" "$CACHE_ROOT/locks" "$CACHE_ROOT/tmp" "$LOG_DIR"
printf '%s' "$BODY_TEXT" > "$BODY_FILE"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  # Keep WORKDIR on failure for debugging; blow it away on success.
  if [[ "${KEEP_WORKDIR:-0}" == "0" && "${TEST_OK:-0}" == "1" ]]; then
    rm -rf "$WORKDIR"
  else
    info "workdir preserved: $WORKDIR"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# HTTP server: single Python process, appends one line per GET /payload to
# hits.log and returns BODY_TEXT. Bound to an ephemeral port; we read the
# chosen port off stdout.
# ---------------------------------------------------------------------------

python3 - "$HIT_LOG" "$BODY_FILE" "$SERVER_LOG" <<'PY' >"$WORKDIR/server.port" &
import http.server
import os
import socketserver
import sys
import threading

hit_log = sys.argv[1]
body_path = sys.argv[2]
srv_log = sys.argv[3]
sys.stderr = open(srv_log, "w", buffering=1)
with open(body_path, "rb") as f:
    body = f.read()

# Global lock so concurrent request-handling threads don't interleave the
# hit-log writes.
lock = threading.Lock()

class Handler(http.server.BaseHTTPRequestHandler):
    def _log(self, method):
        with lock, open(hit_log, "a") as f:
            f.write(f"{method} {self.path}\n")

    def do_GET(self):
        self._log("GET")
        if self.path == "/payload":
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            # Advertise cacheability so downstream tooling behaves; NOT
            # required for the test's assertions.
            self.send_header("Cache-Control", "max-age=3600")
            self.send_header("ETag", '"fixed-etag-42"')
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_error(404, "not this path")

    def do_HEAD(self):
        self._log("HEAD")
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()

    def log_message(self, fmt, *args):
        # Suppress the default per-request line on stderr; we already have
        # hits.log for the count.
        pass

class ReuseServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True
    daemon_threads = True

srv = ReuseServer(("127.0.0.1", 0), Handler)
port = srv.server_address[1]
sys.stdout.write(f"{port}\n")
sys.stdout.flush()
srv.serve_forever()
PY
SERVER_PID=$!

# Wait for the server to write its port. Bounded, so a broken start doesn't
# hang CI forever.
for _ in $(seq 1 50); do
  if [[ -s "$WORKDIR/server.port" ]]; then break; fi
  sleep 0.05
done
[[ -s "$WORKDIR/server.port" ]] || fail "HTTP server did not report a port"
PORT="$(head -n1 "$WORKDIR/server.port")"
[[ "$PORT" =~ ^[0-9]+$ ]] || fail "server port not numeric: $PORT"
URL="http://127.0.0.1:$PORT/payload"
info "server on port $PORT; url=$URL; workers=$N; expected_sha=$EXPECTED_SHA"

# ---------------------------------------------------------------------------
# Spawn N ducklink processes concurrently. Each one:
#   * sees the SAME DUCKLINK_LOCAL_CACHE (so they share the on-disk
#     cache root, and therefore the same lock file);
#   * sees --grant-network all (or the equivalent env) so the wasm
#     TcpStream can reach 127.0.0.1;
#   * loads cache and runs `SELECT cache('<url>')`.
# ---------------------------------------------------------------------------

# `LOAD cache;` resolves via the extension registry (autoload), NOT via
# DuckDB's built-in `LOAD '<path>'` — the latter trips
# "Loading external extensions is disabled through a compile time flag".
# The `cache` extension is autoloaded when the `cache` scalar is called;
# an explicit LOAD is here for symmetry with the memo's example. The
# scalar autoload path is also fine — either works.
SQL="LOAD cache; SELECT cache('${URL}') AS uri;"

export DUCKLINK_LOCAL_CACHE="$CACHE_ROOT"
export DUCKLINK_NETWORK_GRANT="all"

pids=()
for i in $(seq 1 "$N"); do
  out="$LOG_DIR/worker-$i.out"
  err="$LOG_DIR/worker-$i.err"
  ( cd "$ROOT" && "$BIN" --grant-network all -- :memory: -c "$SQL" >"$out" 2>"$err" ) &
  pids+=($!)
done

# Reap all workers, capture rc per worker.
rcs=()
for pid in "${pids[@]}"; do
  if wait "$pid"; then
    rcs+=(0)
  else
    rcs+=($?)
  fi
done

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

# (1) all workers succeeded
bad=0
for i in "${!rcs[@]}"; do
  if [[ "${rcs[$i]}" -ne 0 ]]; then
    worker=$((i + 1))
    bad=$((bad + 1))
    echo "worker $worker exited ${rcs[$i]}:" >&2
    sed 's/^/  err: /' "$LOG_DIR/worker-$worker.err" >&2 || true
    sed 's/^/  out: /' "$LOG_DIR/worker-$worker.out" >&2 || true
  fi
done
[[ "$bad" -eq 0 ]] || fail "$bad of $N ducklink workers failed"

# (2) exactly one GET reached the server
GETS=$(grep -c '^GET /payload$' "$HIT_LOG" 2>/dev/null || echo 0)
info "server observed $GETS GET(s) for /payload"
if [[ "$GETS" -ne 1 ]]; then
  echo "hits.log contents:" >&2
  sed 's/^/  /' "$HIT_LOG" >&2 || true
  fail "expected exactly 1 GET, saw $GETS (file-lock not serialising)"
fi

# (3) every worker's stdout carries the SAME file:// URI
extract_uri() {
  # Grab the first file:// token on stdout. DuckDB shell wraps rows in
  # ASCII art tables; a simple regex tolerates that.
  grep -oE 'file://[^"| ]+' "$1" | head -n1
}

first_uri=""
for i in $(seq 1 "$N"); do
  u="$(extract_uri "$LOG_DIR/worker-$i.out" || true)"
  [[ -n "$u" ]] || {
    sed 's/^/  out: /' "$LOG_DIR/worker-$i.out" >&2
    fail "worker $i did not print a file:// URI"
  }
  if [[ -z "$first_uri" ]]; then
    first_uri="$u"
  elif [[ "$u" != "$first_uri" ]]; then
    fail "worker $i URI mismatch: '$u' vs '$first_uri'"
  fi
done
info "all $N workers returned $first_uri"

# (4) blob hashes to expected sha256
BLOB_PATH="${first_uri#file://}"
[[ -f "$BLOB_PATH" ]] || fail "blob missing at $BLOB_PATH"
GOT_SHA="$($SHA256_CMD "$BLOB_PATH" | awk '{print $1}')"
[[ "$GOT_SHA" == "$EXPECTED_SHA" ]] || \
  fail "blob sha mismatch: expected $EXPECTED_SHA got $GOT_SHA"
info "blob sha256 matches expected ($EXPECTED_SHA)"

# (5) catalog has exactly one row for the URL. The catalog is a sqlite
#     database at <cache_root>/metadata.db; use sqlite3 if available. Skip
#     the check gracefully if sqlite3 isn't installed rather than failing
#     spuriously.
DB="$CACHE_ROOT/metadata.db"
if command -v sqlite3 >/dev/null 2>&1 && [[ -f "$DB" ]]; then
  ROWS=$(sqlite3 "$DB" \
    "SELECT COUNT(*) FROM cache_entries WHERE source_uri = '$URL';")
  [[ "$ROWS" -eq 1 ]] || fail "catalog rows for URL: expected 1, got $ROWS"
  info "catalog has exactly 1 row for $URL"
else
  info "sqlite3 not on PATH (or metadata.db missing); skipping catalog row-count check"
fi

TEST_OK=1
echo "cache-concurrent-miss: PASS (workers=$N, gets=1, uri=$first_uri)"
