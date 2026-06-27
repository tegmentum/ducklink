#!/usr/bin/env python3
"""Provider-neutral conformance runner -- the resolver's hard-gate backbone.

GENERALIZES tooling/smoke.py. Where smoke.py runs `smoke.sql` through the host
CLI and diffs vs `smoke.expected`, this runs a PROVIDER-NEUTRAL conformance
suite (`extensions/<name>-component/conformance.{sql,expected}`) against a chosen
PROVIDER *through the resolver path*, and emits a conformance record

    { "suite": "<name>@<major>", "suite_digest": <sha256>, "at": <wit_contract>,
      "passed": <bool> }

(the abstract shape is {suite, suite_digest, contract_digest, passed}; the
registry serializes contract_digest as the `at` key). The record is what a
registry `providers[]` entry pins and what the resolver's hard gate verifies:
a provider is certified iff `passed && at == wit_contract && suite_digest ==
canonical`, where `canonical` is THIS suite_digest (the resolver recomputes it
from the on-disk suite file via resolver::compute_suite_digest).

What it reuses from the shared engine (datalink_tooling.smoke), unchanged:
  * the CLI argv / host bin / extensions dir         smoke._argv
  * the per-extension env / network grant            smoke._env
  * the prompt-strip + .mode-csv parse               smoke.parse_results
  * the expected-file parse (#, ~~, ?, exact)        smoke.parse_expected/compare
  * the artifact-presence preflight                  smoke._missing_artifacts

What it adds (the conformance-specific bits, factorable into datalink later):
  1. runs conformance.sql (not smoke.sql);
  2. FORCES the provider via the resolver policy (DUCKLINK_EXTENSION_PROVIDER),
     so the suite runs through the resolver candidate pipeline + hard gate;
  3. computes the canonical suite_digest (byte-identical to
     resolver::compute_suite_digest); and
  4. emits / writes the conformance records into registry/index.json providers[].

Usage:
    tooling/conformance.py <name>                 run + print PASS/FAIL + record
    tooling/conformance.py --all                  run every conformance suite
    tooling/conformance.py --list                 list extensions with a suite
    tooling/conformance.py --digest <name>        print the canonical suite_digest
    tooling/conformance.py --provider <id> <name> force a provider (default wasm-component)
    tooling/conformance.py --write-records [<name>|--all]
                                                  persist the records into the registry
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import subprocess
import sys
from pathlib import Path

from datalink_tooling import dlconfig, smoke

CONFIG = str(Path(__file__).resolve().parent / "datalink.config.json")

DEFAULT_PROVIDER = "wasm-component"

# ---------------------------------------------------------------------------
# Suite content digest -- MUST stay byte-identical to
# crates/ducklink-host/src/resolver.rs::compute_suite_digest.
# scheme: sha256( b"duckdb:conformance-suite:1\n" || norm(sql) || b"\n\x1e\n"
#                 || norm(expected) ), hex.  (compose-core compute_digest = sha256)
# ---------------------------------------------------------------------------
SUITE_DOMAIN = b"duckdb:conformance-suite:1\n"
SUITE_SEP = b"\n\x1e\n"


def _normalize_sql(text: str) -> str:
    out = []
    for line in text.splitlines():
        s = line.rstrip()
        if not s.strip():
            continue
        if s.lstrip().startswith("--"):
            continue
        out.append(s)
    return "\n".join(out)


def _normalize_expected(text: str) -> str:
    out = []
    for line in text.splitlines():
        s = line.rstrip()
        if not s.strip():
            continue
        ls = s.lstrip()
        if ls == "#" or ls.startswith("# "):
            continue
        out.append(s)
    return "\n".join(out)


def suite_digest(sql_text: str, expected_text: str) -> str:
    canon = (SUITE_DOMAIN + _normalize_sql(sql_text).encode("utf-8")
             + SUITE_SEP + _normalize_expected(expected_text).encode("utf-8"))
    return hashlib.sha256(canon).hexdigest()


# ---------------------------------------------------------------------------
# Suite files + registry helpers
# ---------------------------------------------------------------------------

def _suite_paths(cfg, name: str) -> tuple[Path, Path]:
    d = smoke.ext_dir(cfg, name)
    return d / "conformance.sql", d / "conformance.expected"


def has_suite(cfg, name: str) -> bool:
    sql, exp = _suite_paths(cfg, name)
    return sql.exists() and exp.exists()


def list_suites(cfg) -> list[str]:
    sc_dir = cfg.get("scaffold", "extensions_dir", default="extensions")
    out = []
    for sql in sorted((cfg.repo_root / sc_dir).glob("*/conformance.sql")):
        if (sql.parent / "conformance.expected").exists():
            out.append(smoke.bare(cfg, sql.parent.name))
    return out


def canonical_digest(cfg, name: str) -> str:
    sql, exp = _suite_paths(cfg, name)
    return suite_digest(sql.read_text(), exp.read_text())


def _registry_path(cfg) -> Path:
    return cfg.path(cfg.get("registry", "index_path", default="registry/index.json"))


def _entries_key(cfg) -> str:
    return cfg.get("registry", "entries_key", default="extensions")


def _find_entry(reg: dict, cfg, name: str) -> dict | None:
    for e in reg.get(_entries_key(cfg), []):
        if e.get("name") == name:
            return e
    return None


def _suite_sql(cfg, name: str) -> str:
    sql_path, _ = _suite_paths(cfg, name)
    comment = cfg.get("smoke", "comment_prefix", default="--")
    body = "\n".join(
        line for line in sql_path.read_text().splitlines()
        if not line.lstrip().startswith(comment)
    )
    return cfg.get("smoke", "sql_preamble", default="") + body


def _run_suite(cfg, name: str, provider: str, timeout: int) -> tuple[bool, str]:
    """Run the suite through the resolver with `provider` forced; return
    (passed, output). The force is the resolver policy knob
    (DUCKLINK_EXTENSION_PROVIDER), applied at preload so the LOAD goes through
    the candidate pipeline + hard gate."""
    if (miss := smoke._missing_artifacts(cfg, name)):
        return (False, miss)
    env = smoke._env(cfg, name)
    env["DUCKLINK_EXTENSION_PROVIDER"] = provider
    try:
        r = subprocess.run(
            smoke._argv(cfg, name), input=_suite_sql(cfg, name),
            capture_output=True, text=True, timeout=timeout,
            cwd=cfg.repo_root, env=env,
        )
    except subprocess.TimeoutExpired:
        return (False, f"timeout after {timeout}s")

    out = r.stdout + r.stderr
    markers = cfg.get("smoke", "panic_markers", default=[])
    if any(m in out for m in markers):
        return (False, out)
    _, exp_path = _suite_paths(cfg, name)
    actual = smoke.parse_results(cfg, r.stdout)
    expected = smoke.parse_expected(exp_path)
    diffs = smoke.compare(actual, expected)
    if diffs:
        msg = ["output mismatch vs conformance.expected:"]
        msg += [f"  {d}" for d in diffs]
        msg.append("--- parsed actual ---")
        msg += [f"  {i+1}: {row}" for i, row in enumerate(actual)]
        return (False, "\n".join(msg))
    return (True, out)


def _abi(cfg) -> str:
    ver = cfg.get("identity", "contract_version", default="2.0.0")
    return f"{cfg.get('identity','contract_package',default='duckdb:extension')}@{ver}"


def _record(cfg, name: str, digest: str, contract: str, passed: bool) -> dict:
    major = cfg.get("identity", "contract_major", default="2")
    return {
        "suite": f"{name}@{major}",
        "suite_digest": digest,
        "at": contract,
        "passed": passed,
    }


def _upsert_records(entry: dict, cfg, name: str, digest: str,
                    contract: str, passed: bool) -> dict:
    """Set the wasm-component (reference) provider's conformance to the measured
    record. Re-pin any OTHER existing providers (e.g. a scaffolded native one) to
    the same canonical suite_digest + contract WITHOUT changing their `passed`
    (only the wasm-component provider was actually executed here). Returns the
    wasm-component record."""
    rec = _record(cfg, name, digest, contract, passed)
    providers = entry.get("providers")
    if not providers:
        entry["providers"] = [{
            "id": "wasm-component",
            "kind": "wasm",
            "reference": True,
            "abi": _abi(cfg),
            "artifact": entry.get("artifact"),
            "content_digest": entry.get("content_digest"),
            "conformance": rec,
        }]
        return rec
    found = False
    for p in providers:
        if p.get("id") == "wasm-component":
            p["conformance"] = rec
            found = True
        elif "conformance" in p:
            # Re-pin to the now-real canonical suite + live contract; leave the
            # provider's own passed verdict untouched (not executed here).
            p["conformance"]["suite_digest"] = digest
            p["conformance"]["at"] = contract
            p["conformance"]["suite"] = rec["suite"]
    if not found:
        providers.insert(0, {
            "id": "wasm-component",
            "kind": "wasm",
            "reference": True,
            "abi": _abi(cfg),
            "artifact": entry.get("artifact"),
            "content_digest": entry.get("content_digest"),
            "conformance": rec,
        })
    return rec


def certify(cfg, name: str, provider: str, timeout: int,
            persist: bool) -> tuple[bool, dict, str]:
    """Bootstrap + verify: write a provisional record so the (now-strict) gate
    admits the provider, run the suite through the resolver, finalize `passed`
    from the real run. Restores the registry unless `persist`. Returns
    (passed, record, output)."""
    reg_path = _registry_path(cfg)
    digest = canonical_digest(cfg, name)
    original = reg_path.read_text()
    reg = json.loads(original)
    entry = _find_entry(reg, cfg, name)
    if entry is None:
        return (False, {}, f"no registry entry for '{name}'")
    contract = entry.get("wit_contract", "")

    # 1. provisional record so the hard gate admits the provider during the run.
    _upsert_records(entry, cfg, name, digest, contract, True)
    reg_path.write_text(json.dumps(reg, indent=2) + "\n")

    # 2. run the suite through the resolver (forced provider).
    passed, output = _run_suite(cfg, name, provider, timeout)

    # 3. finalize the measured verdict.
    reg = json.loads(reg_path.read_text())
    entry = _find_entry(reg, cfg, name)
    rec = _upsert_records(entry, cfg, name, digest, contract, passed)
    final = copy.deepcopy(rec)

    if persist and passed:
        reg_path.write_text(json.dumps(reg, indent=2) + "\n")
    else:
        # non-persist OR a failed run: leave the registry exactly as it was.
        reg_path.write_text(original)
    return (passed, final, output)


def main(argv=None) -> None:
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    dlconfig.add_config_arg(p, default=CONFIG)
    p.add_argument("name", nargs="?")
    p.add_argument("--all", action="store_true")
    p.add_argument("--list", action="store_true")
    p.add_argument("--digest", metavar="NAME")
    p.add_argument("--provider", default=DEFAULT_PROVIDER,
                   help=f"force this provider id (default {DEFAULT_PROVIDER})")
    p.add_argument("--write-records", action="store_true",
                   help="persist the conformance records into the registry")
    p.add_argument("--timeout", type=int, default=60)
    args = p.parse_args(argv)
    cfg = dlconfig.load(args.config)

    if args.digest:
        if not has_suite(cfg, args.digest):
            sys.exit(f"no conformance suite for '{args.digest}'")
        print(canonical_digest(cfg, args.digest))
        return

    if args.list:
        for n in list_suites(cfg):
            print(f"{n}  suite_digest={canonical_digest(cfg, n)[:12]}")
        return

    if not (args.name or args.all):
        p.error("specify <name>, --all, --list, or --digest")

    targets = list_suites(cfg) if args.all else [smoke.bare(cfg, args.name)]
    fails = []
    for name in targets:
        if not has_suite(cfg, name):
            print(f"SKIP  {name}: no conformance suite")
            continue
        passed, rec, output = certify(cfg, name, args.provider,
                                      args.timeout, args.write_records)
        verb = "PASS" if passed else "FAIL"
        persisted = " (written)" if (args.write_records and passed) else ""
        print(f"{verb}  {name} [{args.provider}]{persisted}  "
              f"record={json.dumps(rec)}")
        if not passed:
            fails.append(name)
            for line in output.split("\n")[:30]:
                print(f"    {line}")

    if fails:
        print(f"\n{len(fails)} failed: {', '.join(fails)}")
        sys.exit(1)
    print(f"\nall {len(targets)} certified")


if __name__ == "__main__":
    main()
