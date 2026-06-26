#!/usr/bin/env python3
"""Generate CATALOG.md AND stamp every entry's content-addressed identity.

THIN DELEGATOR + repo-specific generation. The IDENTITY STAMPING (the witcanon
`wit_contract`, `wit_contract_version`, and per-artifact `content_digest`) is
delegated to the shared `datalink_tooling.gen` engine (pip-from-git dependency on
tegmentum/datalink), driven by `tooling/datalink.config.json`. The duplicated
stamping logic that used to live here has been DELETED.

What stays HERE (ducklink-specific) is the human-readable CATALOG.md generation,
injected into the engine as an `extra_outputs` hook so it runs over the
just-stamped registry — preserving the exact previous output and behaviour:

  python3 tooling/gen-catalog.py

  pip install -r requirements-dev.txt   # or: pip install -e ../datalink/tooling
"""
from pathlib import Path

from datalink_tooling import gen

CONFIG = str(Path(__file__).resolve().parent / "datalink.config.json")


def names(coll):
    if isinstance(coll, dict):
        return sorted(coll.keys())
    out = []
    for x in coll or []:
        out.append(x["name"] if isinstance(x, dict) else str(x))
    return out


def fn_list(e):
    return ", ".join(f"`{x}`" for x in e.get("exports", []))


def crates(e):
    cs = e.get("crates") or []
    return ", ".join(cs) if cs else "hand-rolled"


CAT_TITLES = {
    "text-processing": "Text & NLP", "data-types": "Data types & encoding",
    "cryptography": "Cryptography & security", "network": "Network",
    "uncategorized": "Other",
}


def write_catalog(cfg, reg):
    """ducklink-specific: render CATALOG.md from the (already-stamped) registry.

    This is the `extra_outputs` hook the shared gen engine calls with the registry
    dict after identity stamping. Byte-identical to the previous gen-catalog.py
    CATALOG.md output."""
    root = cfg.repo_root

    exts = [e for e in reg["extensions"] if e["name"] != "sample_extension"]

    # group by category (an extension may list several; use the first as primary)
    CATS = {}
    for e in exts:
        cat = (e.get("categories") or ["uncategorized"])[0]
        CATS.setdefault(cat, []).append(e)

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
            if "aggregate" in (e.get("requires") or []):
                tags.append("aggregate")
            if "network" in (e.get("requires") or []):
                tags.append("network")
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

    (root / "CATALOG.md").write_text("\n".join(lines) + "\n")
    print(f"wrote CATALOG.md — {len(exts)} extensions, {total_fns} functions, "
          f"{len(CATS)} categories ({len(agg)} aggregate, {len(net)} network)")


if __name__ == "__main__":
    gen.main(config=CONFIG, extra_outputs=[write_catalog])
