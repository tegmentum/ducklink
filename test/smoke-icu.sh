#!/usr/bin/env bash
# Smoke test: the icu extension (embedded in the wasm DuckDB core) provides
# timezone support (AT TIME ZONE, pg_timezone_names) + collation. Verified
# end-to-end through the ducklink wasm host. icu AUTOLOADS in the wasm core (no
# `LOAD icu;` required).
#
# Golden values captured from native duckdb v1.5.2. TimeZone is pinned to UTC so
# TIMESTAMPTZ rendering is deterministic across hosts: the wasm core defaults to
# UTC (no system TZ under wasi), native defaults to the machine's local TZ. The
# underlying instants are identical; pinning makes the textual output match.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

# Each query is prefixed with SET TimeZone='UTC'; the CLI `-c` renders only the
# final statement's result, so we pipe via stdin and keep one SELECT per check.
run() { printf "SET TimeZone='UTC';\n%s\n" "$1" | "$HOST" -- duckdb-cli :memory: 2>&1 | grep -vE '\[wasi|\[dotcmd'; }

fail=0
check() { local desc="$1" sql="$2" want="$3" out
  out="$(run "$sql")"
  if echo "$out" | grep -qF "$want"; then
    echo "PASS  $desc -> '$want'"
  else
    echo "FAIL  $desc: expected '$want'"; echo "$out" | sed 's/^/      /'; fail=1
  fi
}

check "AT TIME ZONE (TIMESTAMP->TZ)" \
  "SELECT ((TIMESTAMP '2024-01-01 12:00:00' AT TIME ZONE 'America/New_York'))::VARCHAR AS t;" \
  "2024-01-01 17:00:00+00"
check "AT TIME ZONE (TZ->TIMESTAMP)" \
  "SELECT ((TIMESTAMPTZ '2024-06-01 00:00:00+00') AT TIME ZONE 'Asia/Tokyo')::VARCHAR AS tok;" \
  "2024-06-01 09:00:00"
check "pg_timezone_names() populated" \
  "SELECT count(*) > 100 AS has_tz FROM pg_timezone_names();" \
  "true"
check "strptime parse" \
  "SELECT strptime('2024-03-15 09:30', '%Y-%m-%d %H:%M')::VARCHAR AS parsed;" \
  "2024-03-15 09:30:00"
check "NOCASE collation" \
  "SELECT 'a' < 'B' COLLATE NOCASE AS coll;" \
  "true"

if [[ $fail -eq 0 ]]; then echo "ALL PASS  icu"; else echo "FAILURES  icu"; exit 1; fi
