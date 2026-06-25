#!/usr/bin/env bash
# Compose the spatialproj duckdb:extension component with the prebuilt GDAL
# component to produce a self-contained extension artifact.
#
# This proves INTER-COMPONENT COMPOSITION: spatialproj imports gdal:core/srs,
# which is satisfied here by composing gdal.component.wasm (which embeds PROJ +
# proj.db). The result imports only duckdb:extension/* (host-provided at load)
# and wasi/* (host-provided), so the ducklink host loads it like any extension.
#
# NOTE: the prebuilt gdal.component.wasm uses a few WIT identifiers with a
# digit-leading label segment (e.g. `get-extent-3d`, `promote-to-3d`,
# `demote-to-2d`). wasm-tools tolerates them, but the ducklink host's pinned
# wasmtime (39.0.0) rejects them as non-kebab extern names. We rename those
# substrings in the gdal component binary to a same-length kebab-valid form
# (`-3d`->`-d3`, `-2d`->`-d2`) before composing; the rename is consistent across
# the component extern names, core export names, and embedded WIT type section,
# and spatialproj does not call any of the renamed functions.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GDAL="${GDAL_COMPONENT:-$HOME/git/gdal-wasm/build/bin/gdal.component.wasm}"
SP_RAW="$ROOT/target/wasm32-wasip1/release/spatialproj_component.wasm"
OUT="$ROOT/artifacts/extensions/spatialproj.wasm"
GDAL_FIXED="$(mktemp -t gdal-renamed.XXXXXX).wasm"

if [[ ! -f "$SP_RAW" ]]; then
  echo "build the component first:" >&2
  echo "  cargo component build -p spatialproj-component --target wasm32-wasip2 --release" >&2
  exit 1
fi
[[ -f "$GDAL" ]] || { echo "GDAL component not found at $GDAL" >&2; exit 1; }

python3 - "$GDAL" "$GDAL_FIXED" <<'PY'
import sys
src, dst = sys.argv[1], sys.argv[2]
data = open(src, "rb").read()
repls = [
    (b"get-extent-3d", b"get-extent-d3"), (b"get_extent_3d", b"get_extent_d3"),
    (b"promote-to-3d", b"promote-to-d3"), (b"promote_to_3d", b"promote_to_d3"),
    (b"demote-to-2d",  b"demote-to-d2"),  (b"demote_to_2d",  b"demote_to_d2"),
    (b"distance-3d",   b"distance-d3"),   (b"distance_3d",   b"distance_d3"),
    (b"envelope-3d",   b"envelope-d3"),   (b"envelope_3d",   b"envelope_d3"),
    (b"flatten-to-2d", b"flatten-to-d2"), (b"flatten_to_2d", b"flatten_to_d2"),
    (b"set-point-2d",  b"set-point-d2"),  (b"set_point_2d",  b"set_point_d2"),
    (b"add-point-2d",  b"add-point-d2"),  (b"add_point_2d",  b"add_point_d2"),
    (b"is-3d",         b"is-d3"),         (b"is_3d",         b"is_d3"),
]
for a, b in repls:
    assert len(a) == len(b)
    data = data.replace(a, b)
open(dst, "wb").write(data)
PY

wac plug "$SP_RAW" --plug "$GDAL_FIXED" -o "$OUT"
rm -f "$GDAL_FIXED"
echo "composed -> $OUT"
wasm-tools component wit "$OUT" | grep -E "import gdal|import duckdb|export duckdb" || true
