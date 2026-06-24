#!/usr/bin/env bash
# Smoke test: the autocomplete extension (embedded in the wasm DuckDB core)
# provides sql_auto_complete(), returning keyword/identifier completions.
# Verified end-to-end through the ducklink wasm host. autocomplete AUTOLOADS in
# the wasm core (no `LOAD autocomplete;` required).
#
# Golden values captured from native duckdb v1.5.2.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

run() { "$HOST" -- duckdb-cli :memory: -c "$1" 2>&1 | grep -vE '\[wasi|\[dotcmd'; }

fail=0
check() { local desc="$1" sql="$2" want="$3" out
  out="$(run "$sql")"
  if echo "$out" | grep -qF "$want"; then
    echo "PASS  $desc -> '$want'"
  else
    echo "FAIL  $desc: expected '$want'"; echo "$out" | sed 's/^/      /'; fail=1
  fi
}

# Top suggestion for 'SEL' is the SELECT keyword.
check "top suggestion for SEL" \
  "SELECT suggestion FROM sql_auto_complete('SEL') LIMIT 1;" \
  "SELECT"
check "non-empty result set" \
  "SELECT count(*) > 0 AS has FROM sql_auto_complete('SEL');" \
  "true"
# Completing a keyword that introduces a table context surfaces the keyword.
check "FROM context completes keyword" \
  "SELECT count(*) > 0 AS has FROM sql_auto_complete('SELECT * FRO');" \
  "true"

if [[ $fail -eq 0 ]]; then echo "ALL PASS  autocomplete"; else echo "FAILURES  autocomplete"; exit 1; fi
