#!/usr/bin/env bash
# det-build.sh -- deterministic, content-reproducible build of ducklink
# component extensions.
#
# WHY: rustc embeds absolute `$HOME/.cargo/...` and source `file!()`/panic paths
# into the wasm LINEAR-MEMORY DATA SECTION (not custom sections, so
# `wasm-tools strip` alone does NOT remove them). Two builds on different
# machines / checkouts / target dirs therefore produce DIFFERENT bytes -> the
# content-addressed catalog digests diverge on every rebuild.
#
# THE FIX (proven on sqlink #211 / ducklink Task #213): remap every absolute
# path prefix to a stable token, drop debuginfo, pin SOURCE_DATE_EPOCH, build to
# wasm32-wasip2, then `wasm-tools strip --all`. The result is byte-identical
# across machines.
#
# Usage:
#   scripts/det-build.sh <bare-name> [<bare-name> ...]   # build named extensions
#   scripts/det-build.sh --all                           # build every registry entry
#
# Output: artifacts/extensions/<name>.wasm (stripped, deterministic).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

TARGET=wasm32-wasip2
TARGET_DIR="${CARGO_TARGET_DIR:-$HERE/target}"
ART_DIR="$HERE/artifacts/extensions"
mkdir -p "$ART_DIR"

# --- the deterministic recipe -------------------------------------------------
# SOURCE_DATE_EPOCH: 2020-01-01T00:00:00Z, the conventional reproducible-build
# epoch (matches sqlink). remap-path-prefix collapses the two machine-specific
# prefixes ($HOME and the absolute target dir) to stable tokens; -C debuginfo=0
# drops DWARF (a secondary path-leak + nondeterminism source).
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-1577836800}"
export RUSTFLAGS="--remap-path-prefix=$HOME=/home --remap-path-prefix=$TARGET_DIR=/target -C debuginfo=0${RUSTFLAGS:+ $RUSTFLAGS}"

names=()
if [ "${1:-}" = "--all" ]; then
  # every registry entry's bare name (the python is stdlib-only)
  while IFS= read -r n; do names+=("$n"); done < <(
    python3 - <<'PY'
import json
reg = json.load(open("registry/index.json"))
for e in reg["extensions"]:
    if e["name"] != "sample_extension":
        print(e["name"])
PY
  )
else
  names=("$@")
fi

[ "${#names[@]}" -gt 0 ] || { echo "usage: $0 <name> ... | --all" >&2; exit 2; }

built=0
failed=()

# libname <crate-name>: the wasm artifact cargo-component emits is named after
# the crate's [lib] name, NOT the package name. 28 of the ~198 crates set a
# custom [lib] name that drops the `-component` suffix (e.g. httpclient,
# jsonfns), so we MUST read it rather than guess pkg.replace('-','_').
# crate_meta <registry-name>: prints "<package-name>\t<lib-wasm-name>".
# The cargo PACKAGE name and the [lib] name can BOTH differ from the registry
# entry name: e.g. pintest_a (registry) lives in extensions/pintest_a-component/
# but its package is `pintest-a-component` (hyphen); httpclient's [lib] is just
# `httpclient`. Read both from Cargo.toml rather than guessing.
crate_meta() {
  python3 - "$1" <<'PY'
import re, sys, pathlib
n = sys.argv[1]
txt = pathlib.Path(f"extensions/{n}-component/Cargo.toml").read_text()
pkg = re.search(r'^\s*name\s*=\s*"([^"]+)"', txt, re.M).group(1)
m = re.search(r'\[lib\][^\[]*?name\s*=\s*"([^"]+)"', txt, re.S)
lib = m.group(1) if m else pkg.replace('-', '_')
print(f"{pkg}\t{lib}")
PY
}

for n in "${names[@]}"; do
  meta="$(crate_meta "$n")"
  pkg="${meta%%$'\t'*}"
  echo ">> det-build $n ($pkg)"
  ok=1
  cargo component build -p "$pkg" --target "$TARGET" --release >/dev/null 2>&1 || ok=0
  if [ "$ok" = 1 ]; then
    lib="${meta##*$'\t'}"
    src="$TARGET_DIR/$TARGET/release/${lib}.wasm"
    if [ -f "$src" ] && wasm-tools strip --all "$src" -o "$ART_DIR/${n}.wasm" 2>/dev/null; then
      built=$((built + 1))
    else
      echo "   FAILED: $pkg (no artifact $src or strip error)" >&2
      failed+=("$n")
    fi
  else
    echo "   FAILED: $pkg (build)" >&2
    failed+=("$n")
  fi
done

echo "det-build: $built built, ${#failed[@]} failed${failed:+: ${failed[*]}}"
[ "${#failed[@]}" -eq 0 ]
