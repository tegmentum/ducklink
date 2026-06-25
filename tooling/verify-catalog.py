#!/usr/bin/env python3
"""Verify catalog integrity: every registered component extension has a built
artifact, a source crate, a workspace membership, and a smoke test -- and there
are no orphan artifacts. Exit non-zero on any drift.
  python3 tooling/verify-catalog.py
"""
import json, pathlib, sys
ROOT = pathlib.Path(__file__).resolve().parent.parent
reg = json.load(open(ROOT / "registry" / "index.json"))
exts = [e for e in reg["extensions"] if e["name"] != "sample_extension"]
art_dir = ROOT / "artifacts" / "extensions"
ws = (ROOT / "Cargo.toml").read_text()

issues = []
for e in exts:
    n = e["name"]
    if not (art_dir / f"{n}.wasm").exists():
        issues.append(f"{n}: missing artifact artifacts/extensions/{n}.wasm")
    src = ROOT / "extensions" / f"{n}-component"
    if not (src / "src" / "lib.rs").exists():
        issues.append(f"{n}: missing source extensions/{n}-component/src/lib.rs")
    if not (src / "smoke.sql").exists():
        issues.append(f"{n}: missing smoke.sql")
    if f"{n}-component" not in ws:
        issues.append(f"{n}: not a workspace member")
    if not e.get("exports"):
        issues.append(f"{n}: no exports declared")

# orphan artifacts (a .wasm with no registry entry). Server-dependent backends
# (mysqlwasm/postgreswasm/webfs) are intentionally NOT registered -- their smoke
# needs a live external server, so they ship a `smoke.sql.requires-live-server`
# marker instead of a smoke.sql and are excluded from the catalog and `--all`.
registered = {e["name"] for e in exts} | {"sample_extension", "sample-extension", "typetest"}
def is_live_server_backend(name):
    return (ROOT / "extensions" / f"{name}-component" / "smoke.sql.requires-live-server").exists()
for wasm in sorted(art_dir.glob("*.wasm")):
    if wasm.stem not in registered and not is_live_server_backend(wasm.stem):
        issues.append(f"orphan artifact: {wasm.name} (no registry entry)")

agg = [e["name"] for e in exts if "aggregate" in (e.get("requires") or [])]
net = [e["name"] for e in exts if "network" in (e.get("requires") or [])]
print(f"catalog: {len(exts)} component extensions, "
      f"{sum(len(e.get('exports',[])) for e in exts)} functions")
print(f"  scalars + {len(agg)} aggregate ({', '.join(agg)}) + {len(net)} network ({', '.join(net)})")
print(f"  artifacts present: {len(list(art_dir.glob('*.wasm')))}")
if issues:
    print(f"\nFAILED — {len(issues)} issue(s):")
    for i in issues: print(f"  - {i}")
    sys.exit(1)
print("\nOK — registry <-> artifacts <-> source <-> workspace all consistent.")
