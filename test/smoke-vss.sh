#!/usr/bin/env bash
# Smoke test: the vss extension (embedded in the wasm DuckDB core) provides the
# HNSW index type for array similarity search. Verified end-to-end through the
# ducklink wasm host. vss AUTOLOADS in the wasm core when `USING HNSW` is parsed
# (no explicit `LOAD vss;` required).
#
# The array distance scalars (array_distance / array_cosine_similarity /
# array_inner_product) are core_functions and work without vss; they are
# included as a baseline. The vss-specific bit is the `CREATE INDEX ... USING
# HNSW` path, which native duckdb v1.5.2 only exposes after INSTALL/LOAD vss but
# the wasm core has embedded.
#
# Golden values captured from native duckdb v1.5.2 (with vss INSTALL/LOAD for the
# HNSW rows).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

# Single-statement scalars use -c; the HNSW flow needs multiple statements piped
# via stdin (the CLI `-c` renders only the final statement).
run_c() { "$HOST" -- duckdb-cli :memory: -c "$1" 2>&1 | grep -vE '\[wasi|\[dotcmd'; }
run_pipe() { printf '%s' "$1" | "$HOST" -- duckdb-cli :memory: 2>&1 | grep -vE '\[wasi|\[dotcmd'; }

fail=0
check_c() { local desc="$1" sql="$2" want="$3" out
  out="$(run_c "$sql")"
  if echo "$out" | grep -qF "$want"; then echo "PASS  $desc -> '$want'"
  else echo "FAIL  $desc: expected '$want'"; echo "$out" | sed 's/^/      /'; fail=1; fi
}

# --- array distance scalars (core_functions baseline) ---
# Values match native; the host CLI box renderer prints whole floats without a
# trailing ".0" (native -box shows "2.0"), so we assert the integer cast which is
# stable across both renderers.
check_c "array_distance"           "SELECT array_distance([1,2,3]::FLOAT[3], [1,2,5]::FLOAT[3])::INT AS d;" "| 2 "
check_c "array_cosine_similarity"  "SELECT round(array_cosine_similarity([1,0,0]::FLOAT[3], [1,0,0]::FLOAT[3])::DOUBLE, 4)::INT AS c;" "| 1 "
check_c "array_inner_product"      "SELECT array_inner_product([1,2,3]::FLOAT[3], [1,1,1]::FLOAT[3])::INT AS ip;" "| 6 "

# --- HNSW index path (vss-specific) ---
HNSW_SQL=$(cat <<'SQL'
CREATE TABLE t(id INT, v FLOAT[3]);
INSERT INTO t VALUES (1,[1,2,3]),(2,[4,5,6]),(3,[7,8,9]);
CREATE INDEX hidx ON t USING HNSW(v);
SELECT count(*) AS n FROM duckdb_indexes() WHERE index_name='hidx';
SQL
)
out="$(run_pipe "$HNSW_SQL")"
if echo "$out" | grep -qE '\| 1 '; then
  echo "PASS  CREATE INDEX USING HNSW registers index (duckdb_indexes n=1)"
else
  echo "FAIL  HNSW index not registered"; echo "$out" | sed 's/^/      /'; fail=1
fi

# Nearest-neighbour ordering returns the closest row first.
NN_SQL=$(cat <<'SQL'
CREATE TABLE t(id INT, v FLOAT[3]);
INSERT INTO t VALUES (1,[1,2,3]),(2,[4,5,6]),(3,[7,8,9]);
CREATE INDEX hidx ON t USING HNSW(v);
SELECT id FROM t ORDER BY array_distance(v, [1,2,3]::FLOAT[3]) LIMIT 1;
SQL
)
out="$(run_pipe "$NN_SQL")"
if echo "$out" | grep -qE '\| 1 '; then
  echo "PASS  HNSW nearest-neighbour query (closest id=1)"
else
  echo "FAIL  nearest-neighbour query wrong"; echo "$out" | sed 's/^/      /'; fail=1
fi

if [[ $fail -eq 0 ]]; then echo "ALL PASS  vss"; else echo "FAILURES  vss"; exit 1; fi
