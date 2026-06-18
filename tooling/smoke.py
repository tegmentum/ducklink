#!/usr/bin/env python3
"""Run an extension's smoke.sql against the native host CLI + optional assertions.

Each extension owns extensions/<name>-component/smoke.sql. This harness loads the
extension through the *native host* runner (`duckdb-host`, which has a real
component loader) and pipes the file's statements through the CLI's REPL with the
extension preregistered (`--load-extension <name>`).

NOTE on the loader: the wac-composed *standalone* CLI links a no-op loader stub
and cannot instantiate extension components, so smoke runs through `duckdb-host`
(crates/duckdb-component-host) instead. That binary resolves `<name>.wasm` under
artifacts/extensions/, instantiates it with wasmtime, runs its `load()`, and
forwards the captured registrations to the core component.

Failure modes detected:
  * panic / load error / missing function / instantiation failure
    (heuristic match on stdout+stderr)
  * if extensions/<name>-component/smoke.expected exists, the parsed CLI output
    is diffed against it. Mismatches FAIL.

smoke.expected format (one expected output line per CLI output line, in order;
output runs in `.mode csv` so each SELECT emits a header line then its rows):
    plain text     exact match required
    ~~             skip this line (nondet / random / time-of-call)
    ?              any non-empty value accepted
    leading #      comment, ignored

Usage:
    tooling/smoke.py <name>             # smoke one extension
    tooling/smoke.py --build <name>     # build the component + copy artifact, then smoke
    tooling/smoke.py --all              # smoke every ext that has a smoke.sql
    tooling/smoke.py --all --build      # rebuild every component, then smoke all
    tooling/smoke.py --list             # list extensions with smoke.sql
    tooling/smoke.py --seed-expected <name>
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TARGET_DIR = REPO_ROOT / "target" / "wasm32-wasip2" / "release"

HOST_BIN = REPO_ROOT / "target" / "release" / "duckdb-host"
CORE_COMPONENT = TARGET_DIR / "duckdb_core_component.wasm"
CLI_COMPONENT = TARGET_DIR / "duckdb_cli_component.wasm"
EXT_ROOT = REPO_ROOT / "artifacts" / "extensions"

# Strip leading REPL prompts the CLI prints: "D> " and "...> ". Several can chain
# on one line when a multi-line statement is buffered before output appears.
PROMPT_RE = re.compile(r"^(D>\s*|\.\.\.>\s*)+")

WASI_TARGET = "wasm32-wasip2"


def bare(name: str) -> str:
    """Normalize an extension identifier to its bare load name.

    Accepts either the bare name (`isin`) or the crate/dir name
    (`isin-component`) and returns the bare load name used for the registered
    function set + the artifacts/extensions/<name>.wasm file stem.
    """
    return name[: -len("-component")] if name.endswith("-component") else name


def ext_dir(name: str) -> Path:
    return REPO_ROOT / "extensions" / f"{bare(name)}-component"


def find_smoke_files() -> list[Path]:
    return sorted(REPO_ROOT.glob("extensions/*/smoke.sql"))


def build_component(name: str) -> tuple[bool, str]:
    """`cargo component build` the extension and copy its artifact into place."""
    package = f"{bare(name)}-component"
    underscore = bare(name).replace("-", "_")
    built = TARGET_DIR / f"{underscore}_component.wasm"
    cmd = ["cargo", "component", "build", "-p", package, "--target", WASI_TARGET, "--release"]
    result = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        return (False, "\n".join(result.stderr.split("\n")[-30:]))
    if not built.exists():
        return (False, f"expected build output {built.relative_to(REPO_ROOT)} not found")
    EXT_ROOT.mkdir(parents=True, exist_ok=True)
    dest = EXT_ROOT / f"{bare(name)}.wasm"
    shutil.copy2(built, dest)
    return (True, f"built + copied {dest.relative_to(REPO_ROOT)}")


def parse_results(raw: str) -> list[str]:
    """Convert CLI stdout into the ordered list of non-empty output lines.

    Strips leading REPL prompts, drops blank lines. In `.mode csv` each SELECT
    emits one header line then its data row(s); both are returned so
    smoke.expected captures the full sequence (seed it with --seed-expected and
    trim). NULL renders as the literal `NULL`.
    """
    out: list[str] = []
    for line in raw.splitlines():
        stripped = PROMPT_RE.sub("", line).rstrip()
        if not stripped:
            continue
        out.append(stripped)
    return out


def parse_expected(path: Path) -> list[str]:
    """Parse smoke.expected. Comment (`#`/`# ...`) + blank lines ignored. A bare
    `#` glued to a value (e.g. `#ff8800`) is NOT a comment."""
    out: list[str] = []
    for line in path.read_text().splitlines():
        s = line.rstrip()
        if not s.strip():
            continue
        ls = s.lstrip()
        if ls == "#" or ls.startswith("# "):
            continue
        out.append(s)
    return out


def count_smoke_selects(path: Path) -> int:
    """Static count of SELECT statements in smoke.sql (staleness heuristic)."""
    text = path.read_text()
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.DOTALL)
    text = re.sub(r"--[^\n]*", " ", text)
    text = "\n".join(
        line for line in text.splitlines() if not line.lstrip().startswith(".")
    )
    count = 0
    for stmt in text.split(";"):
        if stmt.strip().lower().startswith("select"):
            count += 1
    return count


def staleness(name: str) -> str | None:
    smoke = ext_dir(name) / "smoke.sql"
    expected = ext_dir(name) / "smoke.expected"
    if not smoke.exists() or not expected.exists():
        return None
    n_select = count_smoke_selects(smoke)
    n_expected = len(parse_expected(expected))
    # csv mode emits a header line per SELECT, so a single-column SELECT yields
    # 2 output lines. We don't assert an exact ratio (multi-row results vary);
    # this only flags the obviously-empty case.
    if n_select > 0 and n_expected == 0:
        return f"smoke.sql has {n_select} SELECT(s) but smoke.expected is empty"
    return None


def compare(actual: list[str], expected: list[str]) -> list[str]:
    diffs: list[str] = []
    if len(actual) != len(expected):
        diffs.append(f"length mismatch: actual={len(actual)} lines, expected={len(expected)}")
    for i, (got, want) in enumerate(zip(actual, expected)):
        if want == "~~":
            continue
        if want == "?":
            if not got:
                diffs.append(f"line {i+1}: expected any non-empty value, got empty")
            continue
        if got != want:
            diffs.append(f"line {i+1}: expected {want!r}, got {got!r}")
    return diffs


def _sql_for(name: str) -> str:
    """Read smoke.sql, strip `--` comment lines (the REPL fuses them onto the
    following statement when newlines are collapsed), and force csv mode."""
    smoke = ext_dir(name) / "smoke.sql"
    sql = "\n".join(
        line for line in smoke.read_text().splitlines()
        if not line.lstrip().startswith("--")
    )
    return ".mode csv\n" + sql


def _run_cli(name: str, sql: str, timeout: int) -> subprocess.CompletedProcess:
    argv = [
        str(HOST_BIN),
        "--extensions-dir", str(EXT_ROOT),
        "--", ":memory:",
        "--load-extension", bare(name),
    ]
    return subprocess.run(
        argv, input=sql, capture_output=True, text=True, timeout=timeout, cwd=REPO_ROOT
    )


def smoke_one(name: str, timeout: int = 60) -> tuple[bool, str]:
    smoke = ext_dir(name) / "smoke.sql"
    if not smoke.exists():
        return (False, f"no smoke.sql at {smoke.relative_to(REPO_ROOT)}")
    if not HOST_BIN.exists():
        return (False, f"host runner not built: {HOST_BIN.relative_to(REPO_ROOT)} missing; "
                       f"run: cargo build --release -p duckdb-component-host")
    if not CORE_COMPONENT.exists() or not CLI_COMPONENT.exists():
        return (False, "core/cli components not built; run: make all")
    artifact = EXT_ROOT / f"{bare(name)}.wasm"
    if not artifact.exists():
        return (False, f"extension artifact missing: {artifact.relative_to(REPO_ROOT)}; "
                       f"run: make ext NAME={bare(name)}-component  (or smoke.py --build {bare(name)})")

    sql = _sql_for(name)
    try:
        result = _run_cli(name, sql, timeout)
    except subprocess.TimeoutExpired:
        return (False, f"timeout after {timeout}s")

    out = result.stdout + result.stderr
    panic_markers = (
        "panicked",
        "failed to preload extension",
        "no artifact found",
        "extension instantiation",
        "Catalog Error",
        "Binder Error",
        "Parser Error",
        "did not find function",
        "Function with name",
    )
    if any(m in out for m in panic_markers):
        return (False, out)

    if (stale := staleness(name)):
        out = f"WARN: {stale}\n{out}"

    expected_path = ext_dir(name) / "smoke.expected"
    if not expected_path.exists():
        actual = parse_results(result.stdout)
        data = [r for r in actual]
        if len(data) >= 4 and all(row == "NULL" for row in data):
            out = ("WARN: every parsed line is NULL  is your scalar wired up? "
                   "(no smoke.expected yet; seed one with --seed-expected)\n" + out)

    if expected_path.exists():
        actual = parse_results(result.stdout)
        expected = parse_expected(expected_path)
        diffs = compare(actual, expected)
        if diffs:
            msg = ["output mismatch vs smoke.expected:"]
            msg.extend(f"  {d}" for d in diffs)
            msg.append("--- parsed actual ---")
            msg.extend(f"  {i+1}: {row}" for i, row in enumerate(actual))
            return (False, "\n".join(msg))

    return (True, out)


def seed_expected(name: str, timeout: int) -> None:
    expected = ext_dir(name) / "smoke.expected"
    if expected.exists():
        print(f"smoke.expected already exists at {expected.relative_to(REPO_ROOT)}", file=sys.stderr)
        print("delete it first if you intend to reseed.", file=sys.stderr)
        sys.exit(1)
    smoke = ext_dir(name) / "smoke.sql"
    if not smoke.exists():
        print(f"no smoke.sql at {smoke.relative_to(REPO_ROOT)}", file=sys.stderr)
        sys.exit(1)
    r = _run_cli(name, _sql_for(name), timeout)
    rows = parse_results(r.stdout)
    header = (
        "# AUTO-SEEDED by smoke.py --seed-expected. Review and trim:\n"
        "#   - csv mode emits a header line per SELECT; keep or drop as you like\n"
        "#   - replace nondeterministic lines (timestamps, rng) with ~~\n"
        "#   - replace order-sensitive lines with ? if any-non-empty is OK\n"
        "#   - delete this banner once you've reviewed each line\n"
    )
    expected.write_text(header + "\n".join(rows) + "\n")
    print(f"wrote {len(rows)} lines to {expected.relative_to(REPO_ROOT)}")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("name", nargs="?", help="extension to smoke (bare or -component name)")
    p.add_argument("--all", action="store_true", help="smoke every extension that has smoke.sql")
    p.add_argument("--build", action="store_true", help="cargo component build + copy artifact first")
    p.add_argument("--list", action="store_true", help="list extensions with smoke.sql")
    p.add_argument("--timeout", type=int, default=60)
    p.add_argument("--seed-expected", metavar="NAME",
                   help="write smoke.expected for NAME from current CLI output")
    args = p.parse_args()
    if not (args.name or args.all or args.list or args.seed_expected):
        p.error("specify <name>, --all, --list, or --seed-expected")

    if args.seed_expected:
        if args.build:
            ok, msg = build_component(args.seed_expected)
            print(("OK  " if ok else "FAIL  ") + msg)
            if not ok:
                sys.exit(1)
        seed_expected(args.seed_expected, args.timeout)
        return

    if args.list:
        for f in find_smoke_files():
            has_expected = (f.parent / "smoke.expected").exists()
            stale = staleness(f.parent.name) if has_expected else None
            marker = ""
            if has_expected:
                marker = " [asserted, STALE]" if stale else " [asserted]"
            line = f"{f.parent.name}{marker}"
            if stale:
                line += f"  {stale}"
            print(line)
        return

    if args.all:
        targets = [bare(f.parent.name) for f in find_smoke_files()]
    else:
        targets = [bare(args.name)]

    if args.build:
        for name in targets:
            ok, msg = build_component(name)
            print(("OK  " if ok else "FAIL  ") + f"build {name}: {msg}")
            if not ok:
                sys.exit(1)

    fails: list[str] = []
    for name in targets:
        ok, output = smoke_one(name, args.timeout)
        print(f"{'PASS' if ok else 'FAIL'}  {name}")
        if not ok:
            fails.append(name)
            for line in output.split("\n")[:30]:
                print(f"    {line}")

    if fails:
        print(f"\n{len(fails)} failed: {', '.join(fails)}")
        sys.exit(1)
    print(f"\nall {len(targets)} passed")


if __name__ == "__main__":
    main()
