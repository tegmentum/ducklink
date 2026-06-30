#!/usr/bin/env python3
"""Fold the extracted function-signature sidecar into the catalog ADDITIVELY.

`catalog-sig-extract` (the Rust loader) emits a `{name -> functions[]}` sidecar
JSON. This step inserts each entry's `functions` array into the matching catalog
entry and rewrites the catalog with the SAME serializer the catalog generator
uses (`json.dump(..., indent=2, ensure_ascii=True)` + trailing newline), so that
every pre-existing byte is preserved and the additive `functions` key is the
ONLY change. Run from the ducklink repo root.

  python3 tools/catalog-sig-extract/merge-functions.py \
      --catalog registry/index.json --sidecar functions.json [--check]

  --check   verify (don't write): assert the result differs from the original
            ONLY by the additive `functions` keys, then exit.
"""
import argparse
import json
import sys


def load(path):
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--catalog", default="registry/index.json")
    ap.add_argument("--sidecar", required=True)
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    original_text = load(args.catalog)
    catalog = json.loads(original_text)
    sidecar = json.loads(load(args.sidecar))

    entries = catalog.get("extensions")
    if not isinstance(entries, list):
        print("error: catalog has no `extensions` array", file=sys.stderr)
        return 2

    # Sanity: every name in the original is unique (insertion keyed by name).
    names = [e.get("name") for e in entries]
    if len(names) != len(set(names)):
        print("error: duplicate entry names in catalog", file=sys.stderr)
        return 2

    added = 0
    for e in entries:
        fns = sidecar.get(e.get("name"))
        if fns is None:
            continue
        # Additive: append `functions` (insertion order keeps it last). dict
        # preserves insertion order in py3.7+, so existing keys are untouched.
        e["functions"] = fns
        added += 1

    new_text = json.dumps(catalog, indent=2, ensure_ascii=True) + "\n"

    # Invariant check: parse both and confirm the ONLY delta is additive
    # `functions` keys (no field changed, no entry added/removed/reordered).
    orig = json.loads(original_text)
    new = json.loads(new_text)
    assert list(orig.keys()) == list(new.keys()), "top-level keys changed"
    for k in orig:
        if k == "extensions":
            continue
        assert orig[k] == new[k], f"top-level field {k!r} changed"
    oe, ne = orig["extensions"], new["extensions"]
    assert len(oe) == len(ne), "entry count changed"
    for o, n in zip(oe, ne):
        assert o.get("name") == n.get("name"), "entry order changed"
        for kk in o:
            assert kk in n and o[kk] == n[kk], f"{o.get('name')}: field {kk!r} changed"
        extra = set(n) - set(o)
        assert extra <= {"functions"}, f"{o.get('name')}: unexpected new keys {extra}"

    print(
        f"[merge-functions] entries={len(ne)} enriched={added} "
        f"(additive `functions` only)",
        file=sys.stderr,
    )

    if args.check:
        print("[merge-functions] --check OK (no write)", file=sys.stderr)
        return 0

    with open(args.catalog, "w", encoding="utf-8") as f:
        f.write(new_text)
    print(f"[merge-functions] wrote {args.catalog}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
