#!/usr/bin/env python3
"""Sync the duckdb:extension WIT contract from the upstream canonical source and
propagate the versioned copy everywhere it is consumed inside this repo.

The single source of truth for the duckdb:extension WIT interfaces lives OUT OF
TREE at `~/git/duckdb-wit/wit/duckdb-extension/*.wit` (path overridable via the
`DUCKDB_WIT_ROOT` env var). ducklink and its sibling `ducklink-extension` both
consume the same upstream files so their contracts stay identical.

Inside this repo, `wit/duckdb-extension/*.wit` is the LOCAL MIRROR of the
upstream source, AUGMENTED with ducklink's own dispatch-layer WITs
(`storage-dispatch`, `storage-write-dispatch`, `aggregate-incr-dispatch`,
`register-*`, etc.). Upstream duckdb-wit is intentionally the PURE
`duckdb:extension` engine surface (`*-host` interfaces + `callback-dispatch`);
ducklink adds its batched-dispatch layer on top under the same package version.
Every loadable component carries its OWN frozen copy of (a subset of) those
files under `extensions/<name>/wit/`; the runtime host carries a copy under
`crates/ducklink-runtime/wit/deps/duckdb-extension/`; the standalone/cli/loader
worlds reference the package across deps. A contract bump therefore touches
many files -- this tool makes it ONE command.

The contract version is the single constant CONTRACT_VERSION below. Running the
tool performs three phases in order:

  0. SYNC: copy `${DUCKDB_WIT_ROOT}/wit/duckdb-extension/*.wit` into
     `wit/duckdb-extension/` (byte-for-byte) for the files that exist upstream.
     Local-only files (ducklink's dispatch-layer WITs, plus `deps.toml` and any
     nested `worlds/` files not present upstream) are LEFT UNTOUCHED -- upstream
     is a subset of the local contract surface, not a mirror of it. Skip with
     `--no-sync` for rewrite-only runs (useful when hacking on the local mirror
     before pushing upstream).

  1. PIN PACKAGE HEADER: in every WIT file under the managed roots,
                          package duckdb:extension;
                      ->  package duckdb:extension@<CONTRACT_VERSION>;
     (an already-versioned package line is re-pinned to CONTRACT_VERSION.)

  2. PIN FOREIGN REFS: foreign package references that name the package WITHOUT
     a version, in `use` / `import` / `export` positions,
            use    duckdb:extension/runtime;
            import duckdb:extension/runtime;
        ->  use    duckdb:extension/runtime@<CONTRACT_VERSION>;
            import duckdb:extension/runtime@<CONTRACT_VERSION>;
     A foreign reference MUST carry the version or it will not resolve against a
     versioned dep package (wit resolves by exact package id).

Same-package references (`use types;`, `import runtime;` with no `duckdb:`
prefix) are left alone -- they resolve within the versioned package and need no
suffix.

Usage:
    tooling/propagate-wit.py                # sync upstream + pin, rewrite in place
    tooling/propagate-wit.py --check        # exit 1 if anything would change (CI)
    tooling/propagate-wit.py --no-sync      # rewrite only; skip upstream sync
    DUCKDB_WIT_ROOT=/path/to/duckdb-wit \\
        tooling/propagate-wit.py            # override upstream path
"""

from __future__ import annotations

import argparse
import filecmp
import os
import re
import shutil
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Upstream canonical source of the duckdb:extension WIT interfaces. Overridable
# via env for out-of-tree checkouts (e.g. tegmentum/duckdb-wit at a specific
# tag). Default assumes a sibling checkout of duckdb-wit next to ducklink.
DUCKDB_WIT_ROOT = Path(
    os.environ.get("DUCKDB_WIT_ROOT") or (REPO_ROOT.parent / "duckdb-wit")
).resolve()

UPSTREAM_WIT_DIR = DUCKDB_WIT_ROOT / "wit" / "duckdb-extension"
LOCAL_WIT_MIRROR = REPO_ROOT / "wit" / "duckdb-extension"

# Relpaths under LOCAL_WIT_MIRROR that are ducklink-authored SUPERSETS of the
# upstream file and must NOT be overwritten during sync. `worlds/duckdb-extension.wit`
# in upstream defines the single pure-engine world; ducklink layers ~15 additional
# worlds on top (storage-capable, index-capable, storage-write, ...). Overwriting
# it would silently break every extension that targets those worlds.
LOCAL_ONLY_OVERRIDES: set[str] = {
    "worlds/duckdb-extension.wit",
}

# THE single source of truth for the contract version. Bump this, run the tool,
# rebuild both hosts + all components, and the whole catalog moves in lockstep.
CONTRACT_VERSION = "4.0.0"

PACKAGE = "duckdb:extension"

# WIT roots this tool owns. Globs are resolved relative to REPO_ROOT. The canonical
# contract is listed first so it is the authoritative copy; the rest are copies that
# must stay pinned to it.
MANAGED_GLOBS = [
    "wit/duckdb-extension/**/*.wit",
    "wit/core/*.wit",
    "wit/standalone/*.wit",
    "crates/ducklink-runtime/wit/**/*.wit",
    "extensions/*/wit/**/*.wit",
]

# package duckdb:extension;  /  package duckdb:extension@1.2.3;
PACKAGE_RE = re.compile(
    r"^(\s*package\s+" + re.escape(PACKAGE) + r")(@[0-9A-Za-z.\-+]+)?(\s*;)",
    re.MULTILINE,
)

# use|import|export duckdb:extension/iface  (optionally already @ver), keeping any
# trailing `as alias` / `.{ ... }` / `;` intact via the tail group.
FOREIGN_RE = re.compile(
    r"\b(use|import|export)(\s+)(" + re.escape(PACKAGE) + r"/[A-Za-z0-9\-]+)(@[0-9A-Za-z.\-+]+)?"
)


def rewrite(text: str) -> str:
    text = PACKAGE_RE.sub(lambda m: f"{m.group(1)}@{CONTRACT_VERSION}{m.group(3)}", text)
    text = FOREIGN_RE.sub(
        lambda m: f"{m.group(1)}{m.group(2)}{m.group(3)}@{CONTRACT_VERSION}", text
    )
    return text


def iter_files() -> list[Path]:
    seen: set[Path] = set()
    out: list[Path] = []
    for glob in MANAGED_GLOBS:
        for p in sorted(REPO_ROOT.glob(glob)):
            if p.is_file() and p not in seen:
                seen.add(p)
                out.append(p)
    return out


def _iter_upstream_files() -> list[Path]:
    """Every regular file under UPSTREAM_WIT_DIR, recursive. Preserves relative
    paths so nested `worlds/*.wit` stay nested in the mirror."""
    if not UPSTREAM_WIT_DIR.is_dir():
        return []
    out: list[Path] = []
    for p in sorted(UPSTREAM_WIT_DIR.rglob("*")):
        if p.is_file():
            out.append(p)
    return out


def sync_upstream(check: bool) -> list[Path]:
    """Copy every file from UPSTREAM_WIT_DIR into LOCAL_WIT_MIRROR
    byte-for-byte. Returns the list of local paths that changed (or would
    change in --check mode). Identical files are untouched (mtimes preserved
    for honest build-cache invalidation).

    Local-only files (ducklink dispatch-layer WITs, `deps.toml`, nested
    `worlds/*.wit` not present upstream) are LEFT ALONE. Upstream is the pure
    `duckdb:extension` engine surface; the local mirror is a superset that
    layers ducklink's dispatch WITs into the same package. Pruning would
    delete legitimate ducklink content, so we only add/overwrite files that
    exist upstream.
    """
    if not UPSTREAM_WIT_DIR.is_dir():
        raise SystemExit(
            f"upstream WIT source not found at {UPSTREAM_WIT_DIR} "
            f"(set DUCKDB_WIT_ROOT=/path/to/duckdb-wit to override)"
        )

    changed: list[Path] = []

    for src in _iter_upstream_files():
        rel = src.relative_to(UPSTREAM_WIT_DIR)
        if str(rel) in LOCAL_ONLY_OVERRIDES:
            continue
        dst = LOCAL_WIT_MIRROR / rel
        if dst.is_file() and filecmp.cmp(src, dst, shallow=False):
            continue
        changed.append(dst)
        if not check:
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)

    return changed


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--check",
        action="store_true",
        help="do not write; exit 1 if any file is out of sync or not pinned to the contract version",
    )
    ap.add_argument(
        "--no-sync",
        action="store_true",
        help="skip the upstream sync step (rewrite phase only)",
    )
    args = ap.parse_args()

    rel = lambda p: p.relative_to(REPO_ROOT)

    synced: list[Path] = []
    if not args.no_sync:
        synced = sync_upstream(check=args.check)

    files = iter_files()
    changed: list[Path] = []
    for path in files:
        original = path.read_text()
        updated = rewrite(original)
        if updated != original:
            changed.append(path)
            if not args.check:
                path.write_text(updated)

    if args.check:
        problems = synced + changed
        if problems:
            if synced:
                print(f"upstream drift: {len(synced)} file(s) out of sync with {UPSTREAM_WIT_DIR}:")
                for p in synced:
                    print(f"  {rel(p)}")
            if changed:
                print(f"contract drift: {len(changed)} file(s) not pinned to @{CONTRACT_VERSION}:")
                for p in changed:
                    print(f"  {rel(p)}")
            return 1
        print(
            f"all {len(files)} WIT files in sync with {UPSTREAM_WIT_DIR} and pinned to "
            f"duckdb:extension@{CONTRACT_VERSION}"
        )
        return 0

    if not args.no_sync:
        if synced:
            print(f"synced {len(synced)} file(s) from {UPSTREAM_WIT_DIR}:")
            for p in synced:
                print(f"  {rel(p)}")
        else:
            print(f"already in sync with {UPSTREAM_WIT_DIR}")

    print(f"propagated duckdb:extension@{CONTRACT_VERSION} across {len(files)} WIT file(s)")
    if changed:
        print(f"updated {len(changed)} file(s):")
        for p in changed:
            print(f"  {rel(p)}")
    else:
        print("no version changes (already pinned)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
