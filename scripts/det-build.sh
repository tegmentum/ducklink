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

# --- C cross-compile sysroot (for crates with native C deps) ------------------
# A handful of components pull a C dependency through a `-sys` crate built by
# cc-rs (e.g. sqlitewasm -> libsqlite3-sys -> sqlite3.c). cc-rs invokes the
# host `clang` with `--target=wasm32-wasip1` but NO sysroot, so the C headers
# (`stdio.h`, ...) are unresolved -> build fails. We point cc-rs at a real WASI
# sysroot + the wasi-sdk clang/llvm-ar.
#
# NOTE on the target name: `cargo component build` compiles the guest as
# wasm32-WASIP1 under the hood and then adapts it to wasip2, so cc-rs sees the
# wasip1 target. We set BOTH the wasip1 and wasip2 env triples so the C build is
# wired regardless of which cargo-component picks.
#
# DETERMINISM: clang embeds __FILE__ / debug paths into the C objects exactly
# like rustc does for Rust. -ffile-prefix-map collapses the two machine-specific
# prefixes ($HOME, the build dir) to stable tokens and -g0 drops debuginfo;
# SOURCE_DATE_EPOCH (set above) covers any timestamp the toolchain would embed.
# Result: the C objects -- and thus the final component -- are byte-identical
# across machines / checkouts / target dirs.
#
# WASI_SDK auto-detection: prefer an env override, then a ducklink-local deps
# dir, then sqlink's wasi-sdk (same upstream toolchain). Only wired if found; a
# pure-Rust build does not need it.
if [ -z "${WASI_SDK:-}" ]; then
  for cand in "$HERE/deps/wasi-sdk" "$HOME/git/sqlink/deps/wasi-sdk"; do
    if [ -x "$cand/bin/clang" ]; then WASI_SDK="$cand"; break; fi
  done
fi
if [ -n "${WASI_SDK:-}" ] && [ -x "$WASI_SDK/bin/clang" ]; then
  WASI_SYSROOT="${WASI_SYSROOT:-$WASI_SDK/share/wasi-sysroot}"
  _det_cflags="--sysroot=$WASI_SYSROOT -ffile-prefix-map=$HOME=/home -ffile-prefix-map=$HERE=/build -g0"
  export CC_wasm32_wasip1="$WASI_SDK/bin/clang"
  export AR_wasm32_wasip1="$WASI_SDK/bin/llvm-ar"
  export CFLAGS_wasm32_wasip1="--target=wasm32-wasip1 $_det_cflags"
  export CC_wasm32_wasip2="$WASI_SDK/bin/clang"
  export AR_wasm32_wasip2="$WASI_SDK/bin/llvm-ar"
  export CFLAGS_wasm32_wasip2="--target=wasm32-wasip2 $_det_cflags"
  echo "det-build: C cross-compile wired via WASI_SDK=$WASI_SDK" >&2
else
  echo "det-build: no WASI_SDK found (pure-Rust builds unaffected; C-dep builds will fail)" >&2
fi

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
