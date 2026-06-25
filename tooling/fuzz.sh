#!/usr/bin/env bash
# Run every cargo-fuzz target for the highest-risk adversarial-input parsers for
# a bounded time. The contract under test: NONE of these parsers may PANIC on any
# input (a panic in a parser or the FFI bridge can abort the host).
#
# Targets (fuzz/fuzz_targets/):
#   mysql_parse     - hand-rolled MySQL wire protocol (untrusted server bytes)
#   postgres_parse  - hand-rolled PostgreSQL v3 wire protocol
#   wkb_decode      - little-endian WKB geometry binary decoder (geomtype)
#   hex_decode      - SQLite hex-VARCHAR -> bytes decoder
#   bencode_decode  - BitTorrent bencode -> JSON decoder
#
# Usage:
#   tooling/fuzz.sh                 # 60s per target
#   tooling/fuzz.sh 300             # 300s per target
#   tooling/fuzz.sh 60 wkb_decode   # one target, 60s
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT/fuzz"

SECS="${1:-60}"
shift || true
TARGETS=("$@")
if [ "${#TARGETS[@]}" -eq 0 ]; then
  TARGETS=(mysql_parse postgres_parse wkb_decode hex_decode bencode_decode)
fi

# cargo-fuzz requires a nightly toolchain (libfuzzer + sanitizer support).
TC="${FUZZ_TOOLCHAIN:-nightly}"

rc=0
for t in "${TARGETS[@]}"; do
  echo "==================================================================="
  echo "  fuzzing $t for ${SECS}s"
  echo "==================================================================="
  if ! cargo "+${TC}" fuzz run "$t" -- \
        -max_total_time="$SECS" -rss_limit_mb=4096 -max_len=8192; then
    echo "!! CRASH in $t -- see fuzz/artifacts/$t/"
    rc=1
  fi
done

if [ "$rc" -ne 0 ]; then
  echo "FUZZING FOUND CRASHES (artifacts under fuzz/artifacts/)."
else
  echo "All fuzz targets ran clean (no panics)."
fi
exit "$rc"
