#!/usr/bin/env python3
"""Verify catalog integrity: every registered component extension has a built
artifact, a source crate, a workspace membership, and a smoke test -- and there
are no orphan artifacts. Exit non-zero on any drift.
  python3 tooling/verify-catalog.py
"""
import json, pathlib, re, subprocess, sys
# --no-artifacts: skip the built-.wasm checks (artifact presence + orphans) so the
# registry<->source<->workspace<->prefix consistency can be validated WITHOUT the
# wasm toolchain -- the self-contained subset CI (ci.yml) runs in `act`.
NO_ARTIFACTS = "--no-artifacts" in sys.argv
ROOT = pathlib.Path(__file__).resolve().parent.parent

# The duckdb:extension WIT contract this catalog targets. Mirrors
# CONTRACT_VERSION in tooling/propagate-wit.py and CONTRACT_MAJOR in
# crates/ducklink-runtime/src/lib.rs -- a bump moves all three in lockstep.
CONTRACT_VERSION = "2.0.0"
CONTRACT_MAJOR = CONTRACT_VERSION.split(".")[0]

_IMPORT_RE = re.compile(r"\bimport\s+duckdb:extension/[A-Za-z0-9\-]+(?:@([0-9]+\.[0-9]+\.[0-9]+))?")

def component_contract(wasm_path):
    """The duckdb:extension contract version a built component imports, read from
    `wasm-tools component wit`. Returns the version string (e.g. '2.0.0'),
    'unversioned' for a legacy pre-versioning component, or None if the package
    isn't imported / wasm-tools is unavailable."""
    try:
        out = subprocess.run(
            ["wasm-tools", "component", "wit", str(wasm_path)],
            capture_output=True, text=True, check=True,
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    versioned = None
    saw_unversioned = False
    for m in _IMPORT_RE.finditer(out):
        if m.group(1):
            versioned = m.group(1)
            break
        saw_unversioned = True
    if versioned:
        return versioned
    return "unversioned" if saw_unversioned else None
reg = json.load(open(ROOT / "registry" / "index.json"))
exts = [e for e in reg["extensions"] if e["name"] != "sample_extension"]
art_dir = ROOT / "artifacts" / "extensions"
ws = (ROOT / "Cargo.toml").read_text()

issues = []
for e in exts:
    n = e["name"]
    if not NO_ARTIFACTS and not (art_dir / f"{n}.wasm").exists():
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
    # Contract version: the registry MUST declare wit_contract matching the
    # catalog's target major, so a stale entry can't ride a contract bump.
    wc = e.get("wit_contract")
    if not wc:
        issues.append(f"{n}: missing `wit_contract` (expected {CONTRACT_VERSION})")
    elif wc.split(".")[0] != CONTRACT_MAJOR:
        issues.append(
            f"{n}: wit_contract {wc} != catalog contract major {CONTRACT_MAJOR}.x"
        )
    # When the artifact is present, the BUILT component's actually-imported
    # duckdb:extension version must match its declared wit_contract -- so drift
    # between the registry and the deployed .wasm is caught before deploy.
    if not NO_ARTIFACTS and wc:
        art = art_dir / f"{n}.wasm"
        if art.exists():
            actual = component_contract(art)
            if actual is None:
                pass  # no duckdb:extension import or wasm-tools missing; nothing to assert
            elif actual == "unversioned":
                issues.append(
                    f"{n}: artifact imports an UNVERSIONED duckdb:extension contract "
                    f"(legacy v1) but registry declares wit_contract {wc}; rebuild it"
                )
            elif actual.split(".")[0] != wc.split(".")[0]:
                issues.append(
                    f"{n}: artifact imports duckdb:extension@{actual} but registry "
                    f"declares wit_contract {wc}"
                )

# orphan artifacts (a .wasm with no registry entry). Server-dependent backends
# (mysqlwasm/postgreswasm/webfs) are intentionally NOT registered -- their smoke
# needs a live external server, so they ship a `smoke.sql.requires-live-server`
# marker instead of a smoke.sql and are excluded from the catalog and `--all`.
registered = {e["name"] for e in exts} | {"sample_extension", "sample-extension", "typetest"}
def is_live_server_backend(name):
    return (ROOT / "extensions" / f"{name}-component" / "smoke.sql.requires-live-server").exists()
if not NO_ARTIFACTS:
    for wasm in sorted(art_dir.glob("*.wasm")):
        if wasm.stem not in registered and not is_live_server_backend(wasm.stem):
            issues.append(f"orphan artifact: {wasm.name} (no registry entry)")

# PLAN-prefixes (v1.1 cutover, ENFORCED): every registry entry MUST declare
# prefix + expansion so its functions get a stable qualified `prefix__name` form
# and a global-identity expansion. The full catalog was backfilled (181/181), so
# this is now a hard check -- a new entry without both fails verification (vs the
# v1 load-time fallback prefix=name, expansion=ducklink-internal://name).
for e in exts:
    if not e.get("prefix"):
        issues.append(f"{e['name']}: missing `prefix` (PLAN-prefixes v1.1)")
    if not e.get("expansion"):
        issues.append(f"{e['name']}: missing `expansion` (PLAN-prefixes v1.1)")

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
