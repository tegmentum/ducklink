#!/usr/bin/env python3
"""Embedding-tracking for ducklink builds — the "Bundles" feature (sqlink parity).

A ducklink *build record* names a set of content-hashed embedding members and is
keyed by a `set_hash` over the sorted members (sqlink's data model: named sets,
content-hashed members, a set_hash, and the embed relationship). Unlike sqlink
(SQLite tables __cas_bundle/_member/_binary) this is stored as JSON in
`registry/builds.json` — ducklink's storage idiom (cf. registry/index.json).

A build has TWO embedding layers, which is the ducklink-specific difference:
  1. core_embedded — the wasm CORE's statically embedded set (EMBED_EXTENSIONS at
     build time; lean default = core_functions+parquet, embeds nothing optional).
  2. components — the loaded / autoloaded / COMPOSED component extensions
     (jsonfns autoloaded; LOAD'd extensions; spatialproj COMPOSES gdal via wac).

Record shape (see registry/builds.json _schema):
  { name, kind: core|composed|bundle,
    core_embedded: [ext...],
    components: [{name, artifact, hash}...],
    composed_of: [{name, embeds:[...]}...]   (optional),
    set_hash, created_at }

HASH: members are content-hashed with BLAKE2b-256 (hashlib.blake2b, digest_size=32).
sqlink uses blake3; hashlib has no blake3, so we use blake2b (a keyed BLAKE family
hash, fixed-output, in the stdlib). The set_hash is blake2b-256 over the sorted,
newline-terminated "name\thash\n" member lines — exactly mirroring sqlink's
sorted-member set hash.

Subcommands:
  record NAME --embed "a,b,c" [--component name@artifact ...] [--composed-of x=y,z]
                              [--kind core|composed|bundle] [--from-manifest FILE]
                              [--now EPOCH]
  list
  show NAME
  gen                 — (re)write BUILDS.md
  verify

The env forbids Date.now-style wall-clock reads in scripts, so created_at is taken
from --now (epoch int) when given, else from the OS clock at record time.
"""
import argparse, hashlib, json, pathlib, sys, time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BUILDS = ROOT / "registry" / "builds.json"
ART = ROOT / "artifacts" / "extensions"
INDEX = ROOT / "registry" / "index.json"

SCHEMA = ("Embedding-tracking for ducklink builds (sqlink Bundles parity). Each "
          "record is a NAMED set of content-hashed embedding members keyed by "
          "set_hash. core_embedded = the wasm core's EMBED_EXTENSIONS set; "
          "components = loaded/autoloaded/composed component extensions; "
          "composed_of = inter-component (wac) compositions and what they embed. "
          "Members are BLAKE2b-256 content hashes; set_hash = BLAKE2b-256 over the "
          "sorted newline-terminated 'name\\thash\\n' member lines.")


def hash_bytes(b: bytes) -> str:
    return hashlib.blake2b(b, digest_size=32).hexdigest()


def hash_file(p: pathlib.Path) -> str:
    return hash_bytes(p.read_bytes())


def set_hash(members):
    """blake2b-256 over sorted, newline-terminated 'name\\thash\\n' lines."""
    lines = sorted(f"{name}\t{h}\n" for name, h in members)
    return hash_bytes("".join(lines).encode())


def load_db():
    if BUILDS.exists():
        return json.loads(BUILDS.read_text())
    return {"version": "0.1.0", "updated": "", "_schema": SCHEMA, "builds": []}


def save_db(db):
    db["updated"] = time.strftime("%Y-%m-%d", time.gmtime())
    BUILDS.write_text(json.dumps(db, indent=2) + "\n")


def find(db, name):
    for b in db["builds"]:
        if b["name"] == name:
            return b
    return None


def registry_artifact_hash(name):
    """If a component is registered with a built artifact, return its hash, else None."""
    p = ART / f"{name}.wasm"
    return hash_file(p) if p.exists() else None


def members_of(rec):
    """The (name, hash) pairs that define a record's set_hash.
    Covers both embedding layers: core-embedded extensions (hashed by name, since
    they have no standalone artifact) and component artifacts (content-hashed)."""
    members = []
    for e in rec.get("core_embedded", []):
        members.append((f"core:{e}", hash_bytes(e.encode())))
    for c in rec.get("components", []):
        members.append((f"component:{c['name']}", c.get("hash", "")))
    return members


def build_record(name, kind, embed, components, composed_of, now):
    core_embedded = [x.strip() for x in embed.split(",") if x.strip()] if embed else []
    comps = []
    for spec in components or []:
        if "@" in spec:
            cname, artifact = spec.split("@", 1)
        else:
            cname, artifact = spec, f"artifacts/extensions/{spec}.wasm"
        ap = (ROOT / artifact) if not pathlib.Path(artifact).is_absolute() else pathlib.Path(artifact)
        if not ap.exists():
            sys.exit(f"error: component artifact not found: {artifact}")
        comps.append({"name": cname, "artifact": artifact, "hash": hash_file(ap)})
    composed = []
    for spec in composed_of or []:
        # form: subcomponent=embed1,embed2  (e.g. gdal=PROJ,proj.db)
        if "=" in spec:
            sub, embeds = spec.split("=", 1)
            elist = [x.strip() for x in embeds.split(",") if x.strip()]
        else:
            sub, elist = spec, []
        composed.append({"name": sub, "embeds": elist})
    rec = {
        "name": name,
        "kind": kind,
        "core_embedded": core_embedded,
        "components": comps,
    }
    if composed:
        rec["composed_of"] = composed
    rec["set_hash"] = set_hash(members_of(rec))
    rec["created_at"] = now
    return rec


def from_manifest(path, name, kind, now):
    """Ingest a self-recording manifest (e.g. spatialproj.compose.json from
    compose.sh, or last-core-build.json from the build script)."""
    m = json.loads(pathlib.Path(path).read_text())
    embed = ",".join(m.get("embedded_extensions", []))
    components = []
    composed_of = []
    if m.get("output"):
        out = m["output"]
        ap = (ROOT / out) if not pathlib.Path(out).is_absolute() else pathlib.Path(out)
        if ap.exists():
            components.append(f"{m.get('name', name)}@{out}")
    for c in m.get("composed_of", []):
        composed_of.append(f"{c['name']}=" + ",".join(c.get("embeds", [])))
    return build_record(name, kind, embed, components, composed_of, now)


def cmd_record(args):
    now = args.now if args.now is not None else int(time.time())
    db = load_db()
    if args.from_manifest:
        rec = from_manifest(args.from_manifest, args.name, args.kind, now)
    else:
        rec = build_record(args.name, args.kind, args.embed, args.component,
                           args.composed_of, now)
    existing = find(db, args.name)
    if existing:
        if existing["set_hash"] == rec["set_hash"]:
            # idempotent re-record: refresh last_used_at-style touch (keep created_at)
            rec["created_at"] = existing["created_at"]
            db["builds"] = [rec if b["name"] == args.name else b for b in db["builds"]]
            save_db(db)
            print(f"unchanged: {args.name} ({rec['set_hash'][:16]}…)")
            return
        # name exists with a DIFFERENT set_hash — sqlink's alias-conflict rule
        sys.exit(f"error: build name '{args.name}' already exists with a different "
                 f"set_hash ({existing['set_hash'][:16]}… != {rec['set_hash'][:16]}…). "
                 f"Pick a new name or delete the old record.")
    db["builds"].append(rec)
    save_db(db)
    print(f"recorded: {args.name} [{rec['kind']}] set_hash={rec['set_hash'][:16]}… "
          f"core_embedded={rec['core_embedded'] or '[]'} "
          f"components={len(rec['components'])}")


def cmd_list(args):
    db = load_db()
    builds = db["builds"]
    if not builds:
        print("(no builds recorded — run: python3 tooling/builds.py record ...)")
        return
    hdr = f"{'NAME':<18} {'KIND':<9} {'CORE-EMBEDDED':<26} {'#COMP':>5} {'SET-HASH':<18} {'CREATED':<10}"
    print(hdr)
    print("-" * len(hdr))
    for b in sorted(builds, key=lambda x: x["name"]):
        ce = ",".join(b.get("core_embedded", [])) or "-"
        if len(ce) > 25:
            ce = ce[:24] + "…"
        created = time.strftime("%Y-%m-%d", time.gmtime(b["created_at"]))
        print(f"{b['name']:<18} {b['kind']:<9} {ce:<26} {len(b.get('components', [])):>5} "
              f"{b['set_hash'][:16]:<18} {created:<10}")


def cmd_show(args):
    db = load_db()
    b = find(db, args.name)
    if not b:
        sys.exit(f"error: no build named '{args.name}'")
    print(f"build: {b['name']}")
    print(f"  kind:       {b['kind']}")
    print(f"  set_hash:   {b['set_hash']}")
    print(f"  created_at: {b['created_at']} ({time.strftime('%Y-%m-%d %H:%M:%SZ', time.gmtime(b['created_at']))})")
    print(f"  core_embedded ({len(b.get('core_embedded', []))}): "
          f"{', '.join(b.get('core_embedded', [])) or '(none — lean)'}")
    comps = b.get("components", [])
    print(f"  components ({len(comps)}):")
    for c in comps:
        print(f"    - {c['name']:<16} {c['hash'][:16]}…  {c['artifact']}")
    if b.get("composed_of"):
        print(f"  composed_of ({len(b['composed_of'])}):")
        for c in b["composed_of"]:
            print(f"    - {c['name']:<16} embeds: {', '.join(c.get('embeds', [])) or '(none)'}")


def cmd_verify(args):
    db = load_db()
    issues = []
    reg = json.loads(INDEX.read_text()) if INDEX.exists() else {"extensions": []}
    registered = {e["name"]: e for e in reg.get("extensions", [])}
    for b in db["builds"]:
        # set_hash recomputes
        recomputed = set_hash(members_of(b))
        if recomputed != b["set_hash"]:
            issues.append(f"{b['name']}: set_hash mismatch "
                          f"(stored {b['set_hash'][:16]}… != recomputed {recomputed[:16]}…)")
        for c in b.get("components", []):
            ap = (ROOT / c["artifact"]) if not pathlib.Path(c["artifact"]).is_absolute() \
                else pathlib.Path(c["artifact"])
            if not ap.exists():
                issues.append(f"{b['name']}: component '{c['name']}' artifact missing: {c['artifact']}")
                continue
            actual = hash_file(ap)
            if actual != c["hash"]:
                issues.append(f"{b['name']}: component '{c['name']}' hash drift "
                              f"(recorded {c['hash'][:16]}… != on-disk {actual[:16]}…)")
            # if registered, the registry's artifact should be the same file
            if c["name"] in registered:
                ra = registered[c["name"]].get("artifact")
                if ra and ra != c["artifact"]:
                    # not fatal — just note path mismatch
                    issues.append(f"{b['name']}: component '{c['name']}' artifact path "
                                  f"differs from registry ({c['artifact']} vs {ra})")
    print(f"builds: {len(db['builds'])} record(s), "
          f"{sum(len(b.get('components', [])) for b in db['builds'])} component members")
    if issues:
        print(f"\nFAILED — {len(issues)} issue(s):")
        for i in issues:
            print(f"  - {i}")
        sys.exit(1)
    print("\nOK — every set_hash recomputes; every component artifact present and unchanged.")


def cmd_gen(args):
    db = load_db()
    builds = sorted(db["builds"], key=lambda x: x["name"])
    lines = []
    lines.append("# ducklink builds — embedding tracking\n")
    lines.append("> Auto-generated from `registry/builds.json` by "
                 "`python3 tooling/builds.py gen`. Do not edit by hand.\n")
    lines.append("Each **build** is a named, content-hashed set of embedding members "
                 "(sqlink's *Bundles* model, JSON-storage idiom). A ducklink build has "
                 "two embedding layers: the wasm **core's** statically embedded set "
                 "(`EMBED_EXTENSIONS`; lean default embeds nothing optional) and the "
                 "**component** extensions it loads / autoloads / composes. The "
                 "`set_hash` (BLAKE2b-256 over the sorted `name\\thash` members) keys "
                 "the set.\n")
    lines.append(f"**{len(builds)} build(s) tracked.**\n")
    lines.append("| Build | Kind | Core-embedded | Components | set_hash |")
    lines.append("|---|---|---|---|---|")
    for b in builds:
        ce = ", ".join(f"`{x}`" for x in b.get("core_embedded", [])) or "_(lean)_"
        comps = ", ".join(f"`{c['name']}`" for c in b.get("components", [])) or "—"
        lines.append(f"| **{b['name']}** | {b['kind']} | {ce} | {comps} | "
                     f"`{b['set_hash'][:16]}…` |")
    lines.append("")
    # composed-of detail for any composed builds
    composed = [b for b in builds if b.get("composed_of")]
    if composed:
        lines.append("## Compositions\n")
        lines.append("Inter-component (`wac plug`) compositions and what each "
                     "sub-component embeds:\n")
        for b in composed:
            lines.append(f"- **{b['name']}** composes:")
            for c in b["composed_of"]:
                emb = ", ".join(f"`{e}`" for e in c.get("embeds", [])) or "(nothing)"
                lines.append(f"  - `{c['name']}` — embeds {emb}")
        lines.append("")
    lines.append("## How embedding is recorded\n")
    lines.append("- **Core embedding** — the wasm build script "
                 "(`../duckdb-wasm/scripts/build-libduckdb-wasm.sh`) writes "
                 "`registry/last-core-build.json` with the `EMBED_EXTENSIONS` split + "
                 "the core artifact hash after each build. Ingest with "
                 "`python3 tooling/builds.py record <name> --kind core "
                 "--from-manifest registry/last-core-build.json`.")
    lines.append("- **Compositions** — `extensions/spatialproj-component/compose.sh` "
                 "writes `extensions/spatialproj-component/spatialproj.compose.json` "
                 "after `wac plug`. Ingest with `python3 tooling/builds.py record "
                 "spatialproj --kind composed --from-manifest "
                 "extensions/spatialproj-component/spatialproj.compose.json`.")
    lines.append("- **Ad-hoc bundles** — `python3 tooling/builds.py record <name> "
                 "--embed a,b --component jsonfns@artifacts/extensions/jsonfns.wasm`.\n")
    (ROOT / "BUILDS.md").write_text("\n".join(lines) + "\n")
    print(f"wrote BUILDS.md — {len(builds)} build(s)")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("record", help="record/update a build")
    p.add_argument("name")
    p.add_argument("--kind", default="bundle", choices=["core", "composed", "bundle"])
    p.add_argument("--embed", default="", help="comma list of core-embedded extensions")
    p.add_argument("--component", action="append", default=[],
                   help="component as name@artifact (repeatable); artifact optional")
    p.add_argument("--composed-of", action="append", default=[], dest="composed_of",
                   help="sub-component composition as sub=embed1,embed2 (repeatable)")
    p.add_argument("--from-manifest", help="ingest a *.compose.json / last-core-build.json")
    p.add_argument("--now", type=int, default=None, help="created_at epoch (else OS clock)")
    p.set_defaults(func=cmd_record)

    sub.add_parser("list", help="table of all builds").set_defaults(func=cmd_list)
    s = sub.add_parser("show", help="full detail of one build")
    s.add_argument("name")
    s.set_defaults(func=cmd_show)
    sub.add_parser("gen", help="regenerate BUILDS.md").set_defaults(func=cmd_gen)
    sub.add_parser("verify", help="consistency check").set_defaults(func=cmd_verify)

    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
