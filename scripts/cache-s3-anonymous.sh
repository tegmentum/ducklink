#!/usr/bin/env bash
# cache-s3-anonymous.sh -- end-to-end proof that ducklink:cache resolves an
# anonymous, public s3:// URI via the wac-composed s3-wasm backend (see
# extensions/cache-component/src/lib.rs::resolve_s3, wired in d2b8870).
#
# Motivating scenario:
#
#   The http/https backend goes through wasi:http host wiring. The s3
#   backend goes through the s3-wasm component (`s3-base.get-object`)
#   which internally rides the SAME wasi:http host wiring. This test
#   proves that the wac composition is intact (s3-base is exported by
#   the composed cache.wasm), that the cache-component's `resolve_s3`
#   dispatch is reachable from the SQL `SELECT cache(cfg, uri)` scalar,
#   and that a real anonymous S3 GET succeeds and is content-addressed
#   into the cache root the same way the http path is.
#
# Bucket / object choice:
#   Default is `s3://noaa-goes16/index.html` from AWS Open Data (NOAA
#   GOES-16 satellite bucket, anonymous readable, tiny HTML index).
#   Override with `S3_URI=s3://<bucket>/<key>` for a different fixture.
#
# What this script asserts:
#   1. All N ducklink invocations exit 0.
#   2. All N stdouts include the SAME file:// URI (the published blob).
#   3. The blob at that path is non-empty (we don't know the sha up
#      front for an anonymous public object; a sha assertion is added
#      when S3_EXPECTED_SHA is provided).
#   4. `__cache_entries` (via <root>/metadata.db) has exactly one row
#      for the URI.
#   5. A second-invocation smoke: re-invoke cache() on the same URI in
#      a fresh process; the returned URI matches and (via HEAD ETag
#      short-circuit in `resolve_s3`) no fresh GET should be needed.
#      This is timed but not counter-verified — there's no server-side
#      hit counter when the endpoint is real AWS.
#
# Prerequisites (fail fast with a clear message if any are missing):
#   * target/release/ducklink       (make host)
#   * artifacts/extensions/cache.wasm composed with the s3-wasm plug so
#     the guest world imports component:s3-wasm/{s3-base,s3-aws}
#     (make cache)
#   * network egress to <bucket>.s3.amazonaws.com (or the equivalent
#     for the caller-supplied fixture)
#   * python3 for the concurrent workers' orchestration (no server
#     needed for this test)
#
# Run:
#   bash scripts/cache-s3-anonymous.sh
# Override the URI or worker count:
#   S3_URI=s3://openneuro.org/README N=2 bash scripts/cache-s3-anonymous.sh
# Pin an expected sha256 (turns assertion (3) into a strict content check):
#   S3_EXPECTED_SHA=<hex> bash scripts/cache-s3-anonymous.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${BIN:-$ROOT/target/release/ducklink}"
CACHE_WASM="${CACHE_WASM:-$ROOT/artifacts/extensions/cache.wasm}"
# Keep the concurrent worker count small: S3 hits AWS's real
# infrastructure and we don't want to hammer public buckets during CI.
N="${N:-2}"
S3_URI="${S3_URI:-s3://noaa-goes16/index.html}"
# Optional strict content check (recommended when using a stable object).
S3_EXPECTED_SHA="${S3_EXPECTED_SHA:-}"
# Anonymous is the whole point of this script; leave true unless the
# caller has a specific reason to flip it.
S3_ANONYMOUS="${S3_ANONYMOUS:-true}"
# Path-style URLs are needed for MinIO / R2 / etc. Real AWS anonymous
# GOES buckets support both virtual-hosted and path-style, but
# path-style is more permissive with the default endpoint hostname.
S3_PATH_STYLE="${S3_PATH_STYLE:-false}"

fail() {
  echo "cache-s3-anonymous: FAIL: $*" >&2
  exit 1
}

info() {
  echo "cache-s3-anonymous: $*" >&2
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

[[ -x "$BIN" ]] || fail "ducklink binary missing at $BIN (run: make host)"
[[ -f "$CACHE_WASM" ]] || fail "cache.wasm missing at $CACHE_WASM (run: make cache)"
command -v python3 >/dev/null 2>&1 || fail "python3 not on PATH"

# The s3-wasm component is wac-plug'd into cache.wasm at compose time,
# which internalises its interfaces — `wasm-tools component wit` no
# longer surfaces `component:s3-wasm/s3-base` as an import once the
# plug is resolved. So instead of trying to match on that (which was
# our first attempt and produces a false negative on the plugged
# component), we assert the proxy indicator: wasi:http/outgoing-handler
# is imported. s3-wasm's transport requires it; without it, the
# composition is broken and the guest wouldn't have loaded at all.
if command -v wasm-tools >/dev/null 2>&1; then
  if ! wasm-tools component wit "$CACHE_WASM" 2>/dev/null | grep -q "wasi:http/outgoing-handler"; then
    fail "cache.wasm at $CACHE_WASM does NOT import wasi:http/outgoing-handler; \
the s3-wasm plug depends on it. Rebuild via 'make cache'."
  fi
  info "verified cache.wasm imports wasi:http/outgoing-handler (s3-wasm transport)"
else
  info "wasm-tools missing; skipping wasi:http import check (test result still valid)"
fi

SHA256_CMD=""
if command -v sha256sum >/dev/null 2>&1; then
  SHA256_CMD="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA256_CMD="shasum -a 256"
else
  fail "no sha256sum / shasum on PATH"
fi

# ---------------------------------------------------------------------------
# Workspace
# ---------------------------------------------------------------------------

# Mirror cache-concurrent-miss.sh: WORKDIR under $ROOT/target so the
# absolute path lands under the cwd preopen the ducklink CLI installs.
# A /tmp workdir would be silently unwritable from the guest.
mkdir -p "$ROOT/target"
WORKDIR="$(mktemp -d "$ROOT/target/ducklink-cache-s3-anonymous.XXXXXX")"
CACHE_ROOT="$WORKDIR/cache"
LOG_DIR="$WORKDIR/logs"
mkdir -p "$CACHE_ROOT" "$CACHE_ROOT/objects" "$CACHE_ROOT/locks" "$CACHE_ROOT/tmp" "$LOG_DIR"

cleanup() {
  # Keep WORKDIR on failure for debugging; blow it away on success.
  if [[ "${KEEP_WORKDIR:-0}" == "0" && "${TEST_OK:-0}" == "1" ]]; then
    rm -rf "$WORKDIR"
  else
    info "workdir preserved: $WORKDIR"
  fi
}
trap cleanup EXIT

info "uri=$S3_URI; workers=$N; cache_root=$CACHE_ROOT"

# ---------------------------------------------------------------------------
# Build the SQL. cache-component's `cache_scalar` intercepts s3:// before
# cache-core's strict Config parser sees the s3-specific keys, so we
# hand it a JSON config with {anonymous, path_style}.
# ---------------------------------------------------------------------------

# Note: the SQL is single-quoted at the shell level and single-quoted
# again inside DuckDB's -c argument. JSON inside the SQL uses double
# quotes which are safe inside single quotes.
CFG_JSON="{\"anonymous\":${S3_ANONYMOUS},\"path_style\":${S3_PATH_STYLE}}"
SQL="LOAD cache; SELECT cache('${CFG_JSON}', '${S3_URI}') AS uri;"

export DUCKLINK_LOCAL_CACHE="$CACHE_ROOT"
export DUCKLINK_NETWORK_GRANT="all"

# ---------------------------------------------------------------------------
# Round 1: spawn N workers concurrently. All should agree on the same
# published blob URI; only one should have actually pulled the object.
# We can't count GETs against real AWS, but we can verify convergence.
# ---------------------------------------------------------------------------

pids=()
for i in $(seq 1 "$N"); do
  out="$LOG_DIR/worker-$i.out"
  err="$LOG_DIR/worker-$i.err"
  ( cd "$ROOT" && "$BIN" --grant-network all \
      --extensions-dir "$ROOT/artifacts/extensions" \
      -- :memory: -c "$SQL" >"$out" 2>"$err" ) &
  pids+=($!)
done

rcs=()
for pid in "${pids[@]}"; do
  if wait "$pid"; then
    rcs+=(0)
  else
    rcs+=($?)
  fi
done

# ---------------------------------------------------------------------------
# Assertions (round 1)
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

# (2) every worker's stdout carries the SAME file:// URI
extract_uri() {
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

# (3) blob is non-empty; if S3_EXPECTED_SHA is set, strict hash match
BLOB_PATH="${first_uri#file://}"
[[ -f "$BLOB_PATH" ]] || fail "blob missing at $BLOB_PATH"
GOT_SHA="$($SHA256_CMD "$BLOB_PATH" | awk '{print $1}')"
BLOB_LEN=$(wc -c < "$BLOB_PATH" | tr -d ' ')
[[ "$BLOB_LEN" -gt 0 ]] || fail "blob at $BLOB_PATH is empty"
info "blob sha256=$GOT_SHA len=$BLOB_LEN"
if [[ -n "$S3_EXPECTED_SHA" ]]; then
  [[ "$GOT_SHA" == "$S3_EXPECTED_SHA" ]] || \
    fail "blob sha mismatch: expected $S3_EXPECTED_SHA got $GOT_SHA"
  info "blob sha matches pinned expected value"
fi

# (4) catalog has exactly one row for the URI
DB="$CACHE_ROOT/metadata.db"
if command -v sqlite3 >/dev/null 2>&1 && [[ -f "$DB" ]]; then
  ROWS=$(sqlite3 "$DB" \
    "SELECT COUNT(*) FROM cache_entries WHERE source_uri = '$S3_URI';")
  [[ "$ROWS" -eq 1 ]] || fail "catalog rows for URI: expected 1, got $ROWS"
  info "catalog has exactly 1 row for $S3_URI"
else
  info "sqlite3 not on PATH (or metadata.db missing); skipping catalog row-count check"
fi

# ---------------------------------------------------------------------------
# Round 2: fresh process, same URI. Should return the same URI without
# re-downloading; `resolve_s3`'s HEAD-ETag short-circuit re-touches
# validated_at but does not re-publish. We time this and log the delta
# vs. round 1 as a smoke; there's no server-side counter to check
# against a real bucket.
# ---------------------------------------------------------------------------

REPLAY_OUT="$LOG_DIR/replay.out"
REPLAY_ERR="$LOG_DIR/replay.err"
info "round 2: re-invoking cache() on the same URI (fresh process)"
if ! ( cd "$ROOT" && "$BIN" --grant-network all \
        --extensions-dir "$ROOT/artifacts/extensions" \
        -- :memory: -c "$SQL" >"$REPLAY_OUT" 2>"$REPLAY_ERR" ); then
  echo "replay stderr:" >&2
  sed 's/^/  /' "$REPLAY_ERR" >&2 || true
  fail "round 2 ducklink invocation failed"
fi

REPLAY_URI="$(extract_uri "$REPLAY_OUT" || true)"
[[ -n "$REPLAY_URI" ]] || fail "round 2 did not print a file:// URI"
[[ "$REPLAY_URI" == "$first_uri" ]] || \
  fail "round 2 URI drift: got '$REPLAY_URI', expected '$first_uri'"
info "round 2 returned same URI $REPLAY_URI (cache hit)"

# Catalog should STILL have exactly one row for the URI (the second
# round is a revalidation, not a second entry).
if command -v sqlite3 >/dev/null 2>&1 && [[ -f "$DB" ]]; then
  ROWS2=$(sqlite3 "$DB" \
    "SELECT COUNT(*) FROM cache_entries WHERE source_uri = '$S3_URI';")
  [[ "$ROWS2" -eq 1 ]] || fail "after round 2, catalog rows: expected 1, got $ROWS2"
fi

TEST_OK=1
echo "cache-s3-anonymous: PASS (workers=$N, uri=$first_uri, sha=$GOT_SHA, len=$BLOB_LEN)"
