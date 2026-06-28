#!/usr/bin/env python3
"""Verify catalog integrity. Exit non-zero on any drift.

THIN DELEGATOR + ducklink-specific checks. The content-addressed IDENTITY checks
(`wit_contract` == recomputed witcanon digest, the `wit_contract_version` major
cross-check, the built artifact's imported `duckdb:extension@MAJOR` cross-check,
and — under `--verify-content`/`--strict` — `content_digest` == sha256 of the
deployed .wasm) are delegated to the shared `datalink_tooling.verify` engine
(pip-from-git dependency on tegmentum/datalink), driven by
`tooling/datalink.config.json`. The duplicated identity logic that used to live
here has been DELETED.

What stays HERE (ducklink-specific) and is injected as `extra_checks`:
  * every entry has a built artifact (skipped under --no-artifacts)
  * every entry has a source crate (extensions/<name>-component/src/lib.rs)
  * every entry has a smoke.sql
  * every entry is a workspace member (Cargo.toml)
  * every entry declares exports
  * no orphan artifacts (a .wasm with no registry entry; skipped under
    --no-artifacts; live-server backends are intentionally excluded)
  * every entry declares prefix + expansion (PLAN-prefixes v1.1, ENFORCED)

  python3 tooling/verify-catalog.py
  python3 tooling/verify-catalog.py --verify-content   # + content_digest
  python3 tooling/verify-catalog.py --no-artifacts     # toolchain-free subset

  pip install -r requirements-dev.txt   # or: pip install -e ../datalink/tooling
"""
import sys
from pathlib import Path

from datalink_tooling import verify

CONFIG = str(Path(__file__).resolve().parent / "datalink.config.json")

# --no-artifacts: skip the built-.wasm checks (artifact presence + orphans) so the
# registry<->source<->workspace<->prefix consistency can be validated WITHOUT the
# wasm toolchain (the self-contained subset CI runs in `act`). The shared engine
# reads this same flag for its own artifact-dependent identity checks.
NO_ARTIFACTS = "--no-artifacts" in sys.argv


def ducklink_checks(cfg, entries):
    """ducklink-specific catalog-integrity checks (everything BEYOND the shared
    identity engine): source / smoke / workspace / exports / orphan-artifact /
    prefix+expansion. Receives the FULL registry entry list; returns issues."""
    root = cfg.repo_root
    exts = [e for e in entries if e["name"] != "sample_extension"]
    art_dir = root / "artifacts" / "extensions"
    ws = (root / "Cargo.toml").read_text()

    issues = []
    for e in exts:
        n = e["name"]
        if not NO_ARTIFACTS and not (art_dir / f"{n}.wasm").exists():
            issues.append(f"{n}: missing artifact artifacts/extensions/{n}.wasm")
        src = root / "extensions" / f"{n}-component"
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
    registered = ({e["name"] for e in exts}
                  # v3 capability PoCs (parser=ggsql, optimizer=qopt): prove the
                  # parser/optimizer driving end-to-end; excluded like typetest.
                  | {"sample_extension", "sample-extension", "typetest", "ggsql", "qopt"})

    def is_live_server_backend(name):
        return (root / "extensions" / f"{name}-component"
                / "smoke.sql.requires-live-server").exists()

    if not NO_ARTIFACTS:
        for wasm in sorted(art_dir.glob("*.wasm")):
            if wasm.stem not in registered and not is_live_server_backend(wasm.stem):
                issues.append(f"orphan artifact: {wasm.name} (no registry entry)")

    # PLAN-prefixes (v1.1 cutover, ENFORCED): every registry entry MUST declare
    # prefix + expansion so its functions get a stable qualified `prefix__name`
    # form and a global-identity expansion. The full catalog was backfilled, so
    # this is a hard check -- a new entry without both fails verification.
    for e in exts:
        if not e.get("prefix"):
            issues.append(f"{e['name']}: missing `prefix` (PLAN-prefixes v1.1)")
        if not e.get("expansion"):
            issues.append(f"{e['name']}: missing `expansion` (PLAN-prefixes v1.1)")

    return issues


if __name__ == "__main__":
    verify.main(config=CONFIG, extra_checks=[ducklink_checks])
