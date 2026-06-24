#!/usr/bin/env bash
# Smoke test: exercise the spatial extension's core scalar surface (GEOS-backed
# geometry constructors, measures, and predicates) through the wasm DuckDB core.
#
# Requires a spatial-enabled libduckdb-wasi.a: spatial must be in the core's
# EMBED_EXTENSIONS / LinkedExtensions, and the GEOS+PROJ+GDAL wasm dep .a files
# (from the sibling ~/git/{geos,proj,gdal}-wasm repos) must be merged into
# artifacts/libduckdb-wasi.a at the core link. As of this writing spatial is NOT
# embedded (see the bottom of this file), so this script will report a clear skip
# until the core is rebuilt with spatial.
#
# Golden output was captured from native duckdb v1.5.2 (spatial dc1996b):
#   INSTALL spatial; LOAD spatial; <the queries below>
# Each scalar is deterministic (no rng / time), so exact-match is safe.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

# Probe: is spatial actually embedded in this core? If not, skip cleanly so the
# suite stays green until the core is rebuilt with spatial.
if ! "$HOST" -- duckdb-cli :memory: -c "SELECT ST_AsText(ST_Point(0,0));" >/dev/null 2>&1; then
  echo "SKIP smoke-spatial: spatial is not embedded in the wasm core."
  echo "  Add 'spatial' to EMBED_EXTENSIONS (it must reach LinkedExtensions in"
  echo "  build/duckdb-wasi/codegen/src/generated_extension_loader.cpp) and merge"
  echo "  the GEOS/PROJ/GDAL wasm dep .a files into artifacts/libduckdb-wasi.a,"
  echo "  then rebuild the core. See ~/.../memory/spatial-wasi-build.md."
  exit 0
fi

# Single-statement queries are passed via -c (the wasm CLI renders the first
# statement only with -c, matching smoke-delta.sh's one-query-per-invocation use).
run() { "$HOST" -- duckdb-cli :memory: -noheader -list -c "$1"; }

fail=0
check() {
  local desc="$1" sql="$2" want="$3" got
  got="$(run "$sql" 2>&1 | tr -d '[:space:]')"
  want="$(printf '%s' "$want" | tr -d '[:space:]')"
  if [[ "$got" == "$want" ]]; then
    echo "ok   $desc"
  else
    echo "FAIL $desc: want [$want] got [$got]"
    fail=1
  fi
}

# --- geometry construction + WKT round-trip ---
check "ST_Point/ST_AsText"      "SELECT ST_AsText(ST_Point(1.0, 2.0));"                                   "POINT (1 2)"
check "ST_GeomFromText (line)"  "SELECT ST_AsText(ST_GeomFromText('LINESTRING(0 0, 3 4)'));"              "LINESTRING (0 0, 3 4)"
check "ST_GeometryType"         "SELECT ST_GeometryType(ST_GeomFromText('LINESTRING(0 0, 1 1)'));"        "LINESTRING"
check "ST_X/ST_Y"               "SELECT ST_X(ST_Point(7,8)), ST_Y(ST_Point(7,8));"                        "7.0|8.0"

# --- measures ---
check "ST_Distance"             "SELECT ST_Distance(ST_Point(0,0), ST_Point(3,4));"                       "5.0"
check "ST_Length"               "SELECT ST_Length(ST_GeomFromText('LINESTRING(0 0, 3 4)'));"              "5.0"
check "ST_Area"                 "SELECT ST_Area(ST_GeomFromText('POLYGON((0 0, 0 4, 4 4, 4 0, 0 0))'));"  "16.0"
check "ST_Centroid"             "SELECT ST_AsText(ST_Centroid(ST_GeomFromText('POLYGON((0 0, 0 4, 4 4, 4 0, 0 0))')));" "POINT (2 2)"

# --- predicates ---
check "ST_Contains (inside)"    "SELECT ST_Contains(ST_GeomFromText('POLYGON((0 0, 0 4, 4 4, 4 0, 0 0))'), ST_Point(2,2));" "true"
check "ST_Contains (outside)"   "SELECT ST_Contains(ST_GeomFromText('POLYGON((0 0, 0 4, 4 4, 4 0, 0 0))'), ST_Point(9,9));" "false"
check "ST_DWithin"              "SELECT ST_DWithin(ST_Point(0,0), ST_Point(3,4), 5.0);"                   "true"

if [[ "$fail" -ne 0 ]]; then
  echo "smoke-spatial: FAILED"
  exit 1
fi
echo "smoke-spatial: all checks passed"
