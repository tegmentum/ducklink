#!/usr/bin/env python3
"""Propagate the versioned duckdb:extension WIT contract everywhere.

The canonical contract lives in wit/duckdb-extension/*.wit. Every loadable
component carries its OWN frozen copy of (a subset of) those files under
extensions/<name>/wit/, the runtime host carries a copy under
crates/ducklink-runtime/wit/deps/duckdb-extension/, and the standalone/cli/loader
worlds reference the package across deps. A contract bump therefore touches many
files; this tool makes it ONE command.

The contract version is the single constant CONTRACT_VERSION below. Running this
tool rewrites, in every WIT file under the managed roots:

  1. the package declaration:    package duckdb:extension;
                              ->  package duckdb:extension@<CONTRACT_VERSION>;
     (an already-versioned package line is re-pinned to CONTRACT_VERSION)

  2. foreign package references that name the package WITHOUT a version, in
     `use` / `import` / `export` positions:
            use    duckdb:extension/runtime;
            import duckdb:extension/runtime;
        ->  use    duckdb:extension/runtime@<CONTRACT_VERSION>;
            import duckdb:extension/runtime@<CONTRACT_VERSION>;
     A foreign reference MUST carry the version or it will not resolve against a
     versioned dep package (wit resolves by exact package id).

It does NOT touch same-package references (`use types;`, `import runtime;` with
no `duckdb:` prefix) -- those resolve within the versioned package and need no
suffix.

Usage:
    tooling/propagate-wit.py            # rewrite all managed roots in place
    tooling/propagate-wit.py --check    # exit 1 if anything would change (CI)
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# THE single source of truth for the contract version. Bump this, run the tool,
# rebuild both hosts + all components, and the whole catalog moves in lockstep.
CONTRACT_VERSION = "3.1.0"

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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--check",
        action="store_true",
        help="do not write; exit 1 if any file is not pinned to the contract version",
    )
    args = ap.parse_args()

    files = iter_files()
    changed: list[Path] = []
    for path in files:
        original = path.read_text()
        updated = rewrite(original)
        if updated != original:
            changed.append(path)
            if not args.check:
                path.write_text(updated)

    rel = lambda p: p.relative_to(REPO_ROOT)
    if args.check:
        if changed:
            print(f"contract drift: {len(changed)} file(s) not pinned to @{CONTRACT_VERSION}:")
            for p in changed:
                print(f"  {rel(p)}")
            return 1
        print(f"all {len(files)} WIT files pinned to duckdb:extension@{CONTRACT_VERSION}")
        return 0

    print(f"propagated duckdb:extension@{CONTRACT_VERSION} across {len(files)} WIT file(s)")
    if changed:
        print(f"updated {len(changed)} file(s):")
        for p in changed:
            print(f"  {rel(p)}")
    else:
        print("no changes (already pinned)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
