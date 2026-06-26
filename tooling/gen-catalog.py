#!/usr/bin/env python3
"""Generate CATALOG.md — a human-readable index of every component extension —
from registry/index.json, AND stamp every entry's content-addressed contract
identity (the witcanon digest of the canonical duckdb:extension WIT) into the
registry. Run after adding extensions OR after a WIT change (re-propagate first):
  python3 tooling/gen-catalog.py"""
import json, hashlib, pathlib, datetime
ROOT = pathlib.Path(__file__).resolve().parent.parent

# Human-readable contract version (the runtime-observable proxy major). Mirrors
# CONTRACT_VERSION in tooling/propagate-wit.py and crates/ducklink-runtime.
CONTRACT_VERSION = "2.0.0"


def contract_digest() -> str:
    """The AUTHORITATIVE content-addressed `duckdb:extension` contract identity:
    a witcanon digest — sha256("witcanon:1" || canonical WIT bytes), hex — over
    the canonical wit/duckdb-extension/*.wit files, read in sorted-by-filename
    order and concatenated.

    Byte-identical to compose-core::blobs::compute_wit_digest in
    ~/git/webassembly-component-orchestration (SPEC §4.1) and to the digest the
    runtime embeds at build time (crates/ducklink-runtime/build.rs), so the value
    interoperates and equals what the runtime serves via contract_digest()."""
    wit_dir = ROOT / "wit" / "duckdb-extension"
    buf = b"".join(p.read_bytes() for p in sorted(wit_dir.glob("*.wit")))
    return hashlib.sha256(b"witcanon:1" + buf).hexdigest()


def names(coll):
    if isinstance(coll, dict): return sorted(coll.keys())
    out=[]
    for x in coll or []:
        out.append(x["name"] if isinstance(x, dict) else str(x))
    return out

reg = json.load(open(ROOT / "registry" / "index.json"))

# Stamp the content-addressed contract identity into every entry. The witcanon
# DIGEST is the authoritative `wit_contract`; the human version is kept alongside
# as `wit_contract_version`. All entries share one contract, so all get the same
# digest. Rewrite registry/index.json only if anything changed (keep 2-space
# indent + trailing newline to match the existing file's formatting).
DIGEST = contract_digest()
reg_changed = False
for e in reg["extensions"]:
    if e.get("wit_contract") != DIGEST:
        e["wit_contract"] = DIGEST
        reg_changed = True
    if e.get("wit_contract_version") != CONTRACT_VERSION:
        e["wit_contract_version"] = CONTRACT_VERSION
        reg_changed = True
if reg_changed:
    with open(ROOT / "registry" / "index.json", "w") as fh:
        json.dump(reg, fh, indent=2)
        fh.write("\n")
    print(f"stamped contract digest {DIGEST[:12]}… into registry/index.json")

exts = [e for e in reg["extensions"] if e["name"] != "sample_extension"]

# group by category (an extension may list several; use the first as primary)
CATS = {}
for e in exts:
    cat = (e.get("categories") or ["uncategorized"])[0]
    CATS.setdefault(cat, []).append(e)

CAT_TITLES = {
    "text-processing": "Text & NLP", "data-types": "Data types & encoding",
    "cryptography": "Cryptography & security", "network": "Network",
    "uncategorized": "Other",
}

def fn_list(e):
    return ", ".join(f"`{x}`" for x in e.get("exports", []))

def crates(e):
    cs = e.get("crates") or []
    return ", ".join(cs) if cs else "hand-rolled"

total_fns = sum(len(e.get("exports", [])) for e in exts)
agg = [e for e in exts if "aggregate" in (e.get("requires") or [])]
net = [e for e in exts if "network" in (e.get("requires") or [])]

lines = []
lines.append("# ducklink component-extension catalog\n")
lines.append("> Auto-generated from `registry/index.json` by `tooling/gen-catalog.py`. Do not edit by hand.\n")
lines.append(f"**{len(exts)} component extensions** · **{total_fns} SQL functions** · "
             f"{len(agg)} expose aggregates · {len(net)} require network.\n")
lines.append(
    "Every extension is a Rust `wasm32-wasip2` component implementing the "
    "`duckdb:extension` WIT world. Load at runtime with `LOAD <name>` (artifacts in "
    "`artifacts/extensions/`), or browse them at `ducklink serve`. None overlap "
    "DuckDB built-ins; each is verified by `tooling/smoke.py`.\n")
lines.append("## Capabilities\n")
lines.append("- **Scalars** — the default; pure per-row functions.")
lines.append("- **Aggregates** — " + ", ".join(f"`{e['name']}`" for e in agg) +
             " use the whole-batch `call_aggregate` path.")
lines.append("- **Network** — " + ", ".join(f"`{e['name']}`" for e in net) +
             " need an outbound-network grant (`DUCKLINK_NETWORK_GRANT`), off by default.\n")

lines.append("## Loading & embedding\n")
lines.append("- **Runtime load (every extension):** `LOAD <name>;` pulls "
             "`artifacts/extensions/<name>.wasm` — no core recompile, version-independent. "
             "This is the component model's whole point.")
lines.append("- **Static embed (opt-in):** `ducklink compose --embed <name>` bakes an extension "
             "into the core at build time. Wired today for `isin` (`embed-isin` core feature); "
             "`ducklink compose --list` shows what's embeddable. Most extensions stay "
             "runtime-loaded by design.")
lines.append("- **Network grant:** net extensions are denied by default; opt in with "
             "`--grant-network all` (or a name allowlist), equivalently the "
             "`DUCKLINK_NETWORK_GRANT` env var.\n")


for cat in sorted(CATS, key=lambda c: (-len(CATS[c]), c)):
    title = CAT_TITLES.get(cat, cat.replace("-", " ").title())
    group = sorted(CATS[cat], key=lambda e: e["name"])
    lines.append(f"## {title} ({len(group)})\n")
    lines.append("| Extension | Functions | Backed by | Notes |")
    lines.append("|---|---|---|---|")
    for e in group:
        tags = []
        if "aggregate" in (e.get("requires") or []): tags.append("aggregate")
        if "network" in (e.get("requires") or []): tags.append("network")
        note = ", ".join(tags)
        lines.append(f"| **{e['name']}** | {fn_list(e)} | {crates(e)} | {note} |")
    lines.append("")

# builtins / official for context
bi = reg.get("builtins", [])
oot = reg.get("out_of_tree", [])
if names(bi) or names(oot):
    lines.append("## Also in the registry (not component extensions)\n")
    if names(bi):
        lines.append("**DuckDB built-ins:** " + ", ".join(f"`{n}`" for n in names(bi)) + "\n")
    if names(oot):
        lines.append("**Official C++ extensions** (static-linked via `EMBED_EXTENSIONS`): "
                     + ", ".join(f"`{n}`" for n in names(oot)) + "\n")

open(ROOT / "CATALOG.md", "w").write("\n".join(lines) + "\n")
print(f"wrote CATALOG.md — {len(exts)} extensions, {total_fns} functions, "
      f"{len(CATS)} categories ({len(agg)} aggregate, {len(net)} network)")
