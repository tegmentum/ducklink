#!/usr/bin/env python3
"""Smoke-test the pluggable dot-command components end-to-end through ducklink.

Unlike tooling/smoke.py (which exercises *extension* SQL functions), this drives
the dot-command components in artifacts/dotcmds/ the way a user does: it pipes a
scripted REPL session into the native host runner (`ducklink`), then asserts the
output contains the expected text and none of the error markers.

The host loads every *.wasm under artifacts/dotcmds/ automatically, so this needs
`make all` (core + cli + host) and `make dotcmds` to have run first.

Each case runs against an in-memory database with the working directory preopened
(so file-reading commands like .insert/.memory see the fixtures written there).

Usage:
    python3 tooling/smoke-dotcmd.py            # run every component's case
    python3 tooling/smoke-dotcmd.py schema     # run one component (schema/data/fts/maint/core/greet)
"""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
HOST = REPO / "target" / "release" / "ducklink"
EXT_ROOT = REPO / "artifacts" / "extensions"
DOTCMDS = REPO / "artifacts" / "dotcmds"

# Markers that mean a command failed. A case may whitelist one via `allow`.
ERROR_MARKERS = [
    "Catalog Error", "Parser Error", "Binder Error", "Conversion Error",
    "Constraint Error", "Invalid Error", "Internal Error", "Permission Error",
    "no such", "unknown command", "panicked", "usage: .",
]

PEOPLE_JSON = '[{"id":1,"name":"alice"},{"id":2,"name":"bob"}]\n'


def case(component, script, expect, allow=None):
    return {"component": component, "script": script, "expect": expect, "allow": allow or []}


CASES = [
    case("schema",
         ".create_table t id:int name:text active:bool --pk id\n"
         ".add_column t email text\n"
         ".transform t --rename name:full_name --type active:int\n"
         ".create_view v SELECT * FROM t\n"
         ".views\n"
         ".duplicate t t2\n"
         ".create_table parent id:int --pk id\n"
         ".add_fk t2 id parent\n"
         ".create_table p city:text\n"
         "INSERT INTO p VALUES ('NYC'),('LA'),('NYC');\n"
         ".extract p city --table cities --fk-column city_id\n"
         "SELECT count(*) AS ncities FROM cities;\n"
         ".quit\n",
         ["created table t", "added column email", "transformed t", "created view v",
          "duplicated t -> t2", "added FK t2.id -> parent.id",
          "extracted city into cities", "ncities", "2"]),

    case("data",
         ".insert ppl people.json\n"
         ".rows ppl\n"
         ".convert ppl name upper(name)\n"
         ".rows ppl\n"
         ".memory people.json mem\n"
         "SELECT count(*) AS memcount FROM mem;\n"
         ".create_table o id:int\n"
         ".bulk people.json INSERT INTO o SELECT id FROM data\n"
         "SELECT count(*) AS ocount FROM o;\n"
         ".quit\n",
         ["inserted rows from people.json into ppl", "alice", "ALICE",
          "loaded people.json as temp table mem", "memcount", "ocount", "2"]),

    case("fts",
         ".create_table docs id:int body:text\n"
         "INSERT INTO docs VALUES (1,'the quick brown fox'),(2,'a lazy dog');\n"
         ".enable_fts docs id body\n"
         ".search docs id quick\n"
         ".quit\n",
         ["enabled FTS on docs", "fox"]),

    case("maint",
         ".vacuum\n.analyze\n.checkpoint\n.db_size\n.quit\n",
         ["vacuum complete", "analyze complete", "checkpoint complete"]),

    case("core",
         ".create_table c id:int\n"
         "INSERT INTO c VALUES (1),(2);\n"
         ".tables\n.count c\n.columns c\n.quit\n",
         ["c", "2 row(s) in c", "id"]),

    case("greet", ".greet World\n.quit\n", ["World"]),
]


def run(script: str, cwd: Path) -> str:
    argv = [str(HOST), "--extensions-dir", str(EXT_ROOT), "--", "duckdb-cli", ":memory:"]
    # fts autoloads; grant it the network/home so the extension dir resolves.
    env = {**os.environ, "DUCKLINK_NETWORK_GRANT": "*"}
    p = subprocess.run(argv, input=script, capture_output=True, text=True,
                       timeout=90, cwd=str(cwd), env=env)
    return p.stdout + p.stderr


def check(out: str, c: dict) -> list[str]:
    """Return a list of failure messages (empty == pass)."""
    fails = []
    for want in c["expect"]:
        if want not in out:
            fails.append(f"missing expected text: {want!r}")
    for marker in ERROR_MARKERS:
        if marker in out and marker not in c["allow"]:
            # Find the offending line for a useful message.
            line = next((l for l in out.splitlines() if marker in l), marker)
            fails.append(f"error marker {marker!r} in output: {line.strip()!r}")
    return fails


def main() -> int:
    only = sys.argv[1] if len(sys.argv) > 1 else None

    if not HOST.exists():
        print(f"FAIL: host runner missing ({HOST.relative_to(REPO)}); run: make host")
        return 2
    if not DOTCMDS.is_dir() or not any(DOTCMDS.glob("*.wasm")):
        print(f"FAIL: no dot-command artifacts in {DOTCMDS.relative_to(REPO)}; run: make dotcmds")
        return 2

    cases = [c for c in CASES if not only or c["component"] == only]
    if only and not cases:
        print(f"FAIL: no such component case {only!r}; "
              f"have: {', '.join(sorted({c['component'] for c in CASES}))}")
        return 2

    with tempfile.TemporaryDirectory() as td:
        cwd = Path(td)
        (cwd / "people.json").write_text(PEOPLE_JSON)
        # DuckDB resolves $HOME/.duckdb against the cwd preopen; pre-create it so
        # extension autoload (fts) can write its data dir.
        (cwd / ".duckdb" / "extension_data").mkdir(parents=True, exist_ok=True)

        failed = 0
        for c in cases:
            try:
                out = run(c["script"], cwd)
            except subprocess.TimeoutExpired:
                print(f"FAIL  {c['component']:8} timeout")
                failed += 1
                continue
            fails = check(out, c)
            if fails:
                failed += 1
                print(f"FAIL  {c['component']:8}")
                for f in fails:
                    print(f"        - {f}")
            else:
                print(f"PASS  {c['component']:8} ({len(c['expect'])} assertions)")

    total = len(cases)
    print(f"\n{total - failed}/{total} dot-command components passed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
