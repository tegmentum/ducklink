#!/usr/bin/env bash
# Smoke test: the inet extension (embedded in the wasm DuckDB core) provides the
# INET type + its scalar functions. Verified end-to-end through the ducklink
# wasm host. inet AUTOLOADS in the wasm core (no `LOAD inet;` required); an
# explicit `LOAD inet;` also succeeds but is unnecessary.
#
# Golden values captured from native duckdb v1.5.2. Each INET-typed result is
# cast to VARCHAR because the CLI box renderer prints raw INET/TIMESTAMPTZ values
# blank; the cast forces the textual form that matches native.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

# Run one SQL statement through the wasm host CLI, strip host log noise.
run() { "$HOST" -- duckdb-cli :memory: -c "$1" 2>&1 | grep -vE '\[wasi|\[dotcmd'; }

fail=0
check() { # desc | sql | expected-substring
  local desc="$1" sql="$2" want="$3" out
  out="$(run "$sql")"
  if echo "$out" | grep -qF "$want"; then
    echo "PASS  $desc -> '$want'"
  else
    echo "FAIL  $desc: expected '$want'"; echo "$out" | sed 's/^/      /'; fail=1
  fi
}

check "INET literal cast"      "SELECT ('127.0.0.1'::INET)::VARCHAR AS a;"                       "127.0.0.1"
check "host()"                 "SELECT host('192.168.1.5/24'::INET) AS h;"                       "192.168.1.5"
check "netmask()"              "SELECT netmask('192.168.1.5/24'::INET)::VARCHAR AS nm;"          "255.255.255.0/24"
check "network()"              "SELECT network('192.168.1.5/24'::INET)::VARCHAR AS nw;"          "192.168.1.0/24"
check "broadcast()"            "SELECT broadcast('192.168.1.5/24'::INET)::VARCHAR AS bc;"        "192.168.1.255/24"
check "<<= containment"        "SELECT '192.168.1.5'::INET <<= '192.168.1.0/24'::INET AS c;"     "true"
check "IPv6 literal"           "SELECT ('::1'::INET)::VARCHAR AS v6;"                            "::1"

if [[ $fail -eq 0 ]]; then echo "ALL PASS  inet"; else echo "FAILURES  inet"; exit 1; fi
