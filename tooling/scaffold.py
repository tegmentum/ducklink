#!/usr/bin/env python3
"""Scaffold a new DuckDB-wasm component extension.

Consumes tooling/templates/*.tmpl and tooling/compat-registry.json to produce a
working skeleton under extensions/<name>-component/. After scaffolding, registers
the crate as a workspace member and runs `cargo check --target wasm32-wasip2` to
confirm the skeleton compiles before the caller starts editing.

DuckDB extensions register imperatively in `load()` (vs sqlite-wasm's static
`describe()` manifest), so the generated lib.rs opens the host's scalar
capability registry and registers a placeholder scalar + the callback-dispatch
export. The surrounding tooling (compat-registry, smoke, t-status) is shared.

Usage:
    tooling/scaffold.py <name> [--crate crate1,crate2,...] [--description "..."]
    tooling/scaffold.py --list-broken     # show crates flagged broken/needs-bootstrap
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TEMPLATES = REPO_ROOT / "tooling" / "templates"
REGISTRY = REPO_ROOT / "tooling" / "compat-registry.json"
WIT_SOURCE = REPO_ROOT / "extensions" / "sample-extension-component" / "wit"
WORKSPACE_TOML = REPO_ROOT / "Cargo.toml"


def load_registry() -> dict:
    with REGISTRY.open() as f:
        return json.load(f)


def check_crate(name: str, registry: dict) -> tuple[str, str]:
    """Return (status, notes) for `name`. Status is 'unverified' if unseen."""
    entry = registry["crates"].get(name)
    if not entry:
        return ("unverified", "")
    return (entry.get("status", "unverified"), entry.get("notes", ""))


def render(template_name: str, **vars_: str) -> str:
    raw = (TEMPLATES / template_name).read_text()
    return raw.format(**vars_)


def crate_block(crate_specs: list[str], registry: dict) -> tuple[str, list[str]]:
    """Render the [dependencies] block lines for the user's --crate list.

    Returns (block, warnings).
    """
    lines: list[str] = []
    warnings: list[str] = []
    for spec in crate_specs:
        # spec may be "name" or "name@version"
        if "@" in spec:
            name, ver = spec.split("@", 1)
        else:
            name, ver = spec, None
        status, notes = check_crate(name, registry)
        if status == "broken":
            warnings.append(f"  x {name}: BROKEN  {notes}")
            lines.append(f"# BROKEN per compat-registry: {notes}")
            lines.append(f'# {name} = "{ver or "*"}"')
            continue
        if status == "hand-roll-preferred":
            warnings.append(f"  ~ {name}: hand-roll preferred  {notes}")
            lines.append(f"# hand-roll preferred per compat-registry: {notes}")
            continue
        if status == "needs-bootstrap":
            warnings.append(f"  ! {name}: needs RUSTC_BOOTSTRAP=1  {notes}")
        elif status == "needs-feature-tweak":
            warnings.append(f"  ! {name}: needs feature tweak  {notes}")
        elif status == "unverified":
            warnings.append(f"  ? {name}: unverified  evaluate before relying on it")
        if notes:
            lines.append(f"# {notes}")
        version_str = ver or _suggest_version(name, registry)
        lines.append(f'{name} = "{version_str}"')
    return ("\n".join(lines) if lines else "# add your upstream crate deps here", warnings)


def _suggest_version(name: str, registry: dict) -> str:
    entry = registry["crates"].get(name)
    if not entry or "version_tested" not in entry:
        return "*"
    v = entry["version_tested"]
    parts = v.split(".")
    if len(parts) >= 2:
        return f"{parts[0]}.{parts[1]}"
    return parts[0]


def register_workspace_member(member: str) -> bool:
    """Insert `member` into the [workspace].members array of the root Cargo.toml.

    Returns True if added, False if already present. Keeps the array sorted-ish
    by appending before the closing bracket.
    """
    text = WORKSPACE_TOML.read_text()
    if f'"{member}"' in text:
        return False
    lines = text.splitlines()
    in_members = False
    for idx, line in enumerate(lines):
        if re.match(r"\s*members\s*=\s*\[", line):
            in_members = True
            continue
        if in_members and line.strip().startswith("]"):
            indent = "    "
            lines.insert(idx, f'{indent}"{member}",')
            WORKSPACE_TOML.write_text("\n".join(lines) + "\n")
            return True
    raise SystemExit("error: could not locate [workspace].members array in Cargo.toml")


def scaffold_extension(name: str, crates: list[str], description: str) -> None:
    if not re.match(r"^[a-zA-Z][a-zA-Z0-9-]*$", name):
        sys.exit(f"error: extension name must be alphanumeric + hyphens (got {name!r})")
    if name.endswith("-component"):
        sys.exit("error: pass the bare name; '-component' is appended automatically")

    package = f"{name}-component"
    target = REPO_ROOT / "extensions" / package
    if target.exists():
        sys.exit(f"error: {target.relative_to(REPO_ROOT)} already exists")
    if not WIT_SOURCE.is_dir():
        sys.exit(f"error: WIT source {WIT_SOURCE.relative_to(REPO_ROOT)} not found")

    registry = load_registry()
    deps_block, warnings = crate_block(crates, registry)

    target.mkdir(parents=True)
    (target / "src").mkdir()
    # Each extension carries its own copy of the duckdb:extension WIT so the
    # wit_bindgen::generate! macro and cargo-component can resolve the world
    # without reaching across the tree.
    shutil.copytree(WIT_SOURCE, target / "wit")

    name_underscore = name.replace("-", "_")
    desc_short = description.splitlines()[0][:200] if description else f"{name} scalars"

    (target / "Cargo.toml").write_text(
        render(
            "Cargo.toml.tmpl",
            NAME=name,
            DESCRIPTION=description or f"{name} extension",
            DEPS=deps_block,
        )
    )
    (target / "src" / "lib.rs").write_text(
        render(
            "lib.rs.tmpl",
            NAME=name,
            NAME_UNDERSCORE=name_underscore,
            DESCRIPTION_SHORT=desc_short,
        )
    )
    (target / "smoke.sql").write_text(
        render(
            "smoke.sql.tmpl",
            NAME=name,
            NAME_UNDERSCORE=name_underscore,
        )
    )

    print(f"created {target.relative_to(REPO_ROOT)}/Cargo.toml")
    print(f"created {target.relative_to(REPO_ROOT)}/src/lib.rs")
    print(f"created {target.relative_to(REPO_ROOT)}/smoke.sql")
    print(f"copied  {(target / 'wit').relative_to(REPO_ROOT)}/  (duckdb:extension world)")

    if register_workspace_member(f"extensions/{package}"):
        print(f"registered extensions/{package} as a workspace member")
    else:
        print(f"extensions/{package} already a workspace member")

    if warnings:
        print("\ncompat notes:")
        for w in warnings:
            print(w)

    # Build-check to confirm the skeleton compiles before the caller edits.
    if shutil.which("cargo"):
        print(f"\nrunning: cargo check -p {package} --target wasm32-wasip2 --release")
        result = subprocess.run(
            ["cargo", "check", "-p", package, "--target", "wasm32-wasip2", "--release"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            print("FAILED  build-check exited non-zero")
            print("\n".join(result.stderr.split("\n")[-30:]))
            sys.exit(1)
        print("OK  skeleton compiles clean")

    print("\nnext:")
    print(f"  1. edit extensions/{package}/src/lib.rs  add your real scalars")
    print(f"  2. edit extensions/{package}/smoke.sql   add real test inputs")
    print(f"  3. make ext NAME={package}               build + component-wrap + smoke")


def list_broken() -> None:
    registry = load_registry()
    rows = []
    for crate, entry in registry["crates"].items():
        status = entry.get("status", "unverified")
        if status not in ("clean", "unverified"):
            rows.append((status, crate, entry.get("notes", "")[:80]))
    rows.sort()
    if not rows:
        print("no flagged crates  registry is clean")
        return
    width = max(len(c) for _, c, _ in rows)
    for status, crate, note in rows:
        print(f"  {status:20s} {crate:<{width}}  {note}")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("name", nargs="?", help="extension name (bare, '-component' appended)")
    p.add_argument(
        "--crate",
        default="",
        help="comma-separated upstream crate names to wire into deps; appends '@x.y' to pin",
    )
    p.add_argument("--description", default="", help="multi-line description for Cargo.toml")
    p.add_argument(
        "--list-broken",
        action="store_true",
        help="list crates flagged in compat-registry; exits without scaffolding",
    )
    args = p.parse_args()

    if args.list_broken:
        list_broken()
        return

    if not args.name:
        p.error("the following arguments are required: name")

    crates = [c.strip() for c in args.crate.split(",") if c.strip()]
    scaffold_extension(args.name, crates, args.description)


if __name__ == "__main__":
    main()
