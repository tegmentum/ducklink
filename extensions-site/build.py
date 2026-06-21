#!/usr/bin/env python3
"""Build the ducklink extension-registry database.

Mirrors sqlite-wasm's extensions-site: the whole site is a single DuckDB file
(`registry.db`) served by `ducklink serve --db extensions-site/registry.db`.
The httpd's `routes` table drives every request; handlers are DuckDB SQL that
look data up in the same database and return the HTTP response (sql / static /
blob route kinds). Pages + JSON are baked at build time (no json/group_concat at
request time), so a lean core serves it.

Routes:
    /                       landing page with client-side search
    /ext/<name>             per-extension detail
    /api/extensions.json    full registry as JSON
    /api/ext/<name>.json    per-extension as JSON
    /asset/<name>           raw component .wasm bytes (in-DB CAS)
    /asset-info/<name>      { name, size_bytes, sha256, content_type, version, url }
    /static/ducklink_logo.png   the logo
    /health                 ok

Source of truth: registry/index.json (component extensions + the DuckDB builtins
+ official out-of-tree extensions) and the built artifacts under
artifacts/extensions/. The DuckDB writer must match the wasm core's storage
version (1.4.0) so `ducklink serve` can open the file.
"""
import hashlib
import html
import json
import sys
from pathlib import Path

import duckdb

SITE = Path(__file__).resolve().parent
REPO = SITE.parent
TEMPLATES = SITE / "templates"
LOGO = REPO / "docs" / "assets" / "ducklink_logo.png"
REGISTRY = REPO / "registry" / "index.json"
ARTIFACTS = REPO / "artifacts" / "extensions"
OUT = SITE / "registry.db"


def esc(s: str) -> str:
    return html.escape("" if s is None else str(s))


def sql_str(s: str) -> str:
    """Single-quote a string literal for embedding in a route handler."""
    return "'" + str(s).replace("'", "''") + "'"


# ---------------------------------------------------------------------------
# Load + normalize the catalog into a flat list of entries.
# ---------------------------------------------------------------------------
def load_entries() -> tuple[list[dict], dict]:
    cat = json.loads(REGISTRY.read_text())
    categories = cat.get("categories", {})
    entries: list[dict] = []

    # Component extensions (Rust, duckdb:extension world).
    for e in cat.get("extensions", []):
        entries.append({
            "name": e["name"],
            "kind": "component",
            "status": e.get("status", "planned"),
            "description": e.get("description", ""),
            "exports": e.get("exports", []),
            "categories": e.get("categories", []),
            "version": e.get("version"),
            "repository": e.get("repository"),
            "source": e.get("source"),
            "artifact": e.get("artifact"),
        })

    # DuckDB builtins (in-tree C++, statically linked into the core).
    for name, info in cat.get("builtins", {}).items():
        if name.startswith("_"):
            continue
        entries.append({
            "name": name,
            "kind": "builtin",
            "status": info.get("status", "working"),
            "description": info.get("notes", ""),
            "exports": [],
            "categories": ["builtin"],
            "version": None,
            "repository": None,
            "source": "duckdb-in-tree",
            "artifact": None,
        })

    # Official out-of-tree C++ extensions (httpfs, spatial, ...).
    for name, info in cat.get("out_of_tree", {}).items():
        if name.startswith("_"):
            continue
        entries.append({
            "name": name,
            "kind": "official",
            "status": info.get("verdict", "planned"),
            "description": info.get("notes", info.get("deps", "")),
            "exports": [],
            "categories": ["official"],
            "version": None,
            "repository": None,
            "source": "duckdb-out-of-tree",
            "artifact": None,
        })

    entries.sort(key=lambda e: (e["kind"] != "component", e["name"].lower()))
    return entries, {"registry_url": cat.get("registry_url", ""), "categories": categories}


def badge_class(status: str) -> str:
    s = (status or "").lower()
    return s if s in ("working", "planned", "scaffolded") else "other"


# ---------------------------------------------------------------------------
# Asset collection (built component .wasm artifacts -> in-DB CAS).
# ---------------------------------------------------------------------------
def collect_assets() -> dict[str, dict]:
    assets: dict[str, dict] = {}
    if not ARTIFACTS.is_dir():
        return assets
    for f in sorted(ARTIFACTS.glob("*.wasm")):
        blob = f.read_bytes()
        name = f.stem
        assets[name] = {
            "name": name,
            "blob": blob,
            "size_bytes": len(blob),
            "sha256": hashlib.sha256(blob).hexdigest(),
            "content_type": "application/wasm",
        }
    return assets


def artifact_name(entry: dict) -> str | None:
    """Asset name (file stem) for an entry, if a built artifact exists."""
    art = entry.get("artifact")
    if art:
        return Path(art).stem
    return None


# ---------------------------------------------------------------------------
# Rendering.
# ---------------------------------------------------------------------------
def render_install(entry: dict, has_asset: bool) -> str:
    name = esc(entry["name"])
    lines = []
    if entry["kind"] == "component":
        if has_asset:
            stem = artifact_name(entry)
            lines.append(f'<span class="c"># download the component</span>')
            lines.append(f"curl -O /asset/{esc(stem)}")
            lines.append("")
        lines.append(f'<span class="c"># load at runtime</span>')
        lines.append(f"LOAD {name};")
        lines.append("")
        lines.append(f'<span class="c"># or embed it into the core</span>')
        lines.append(f"ducklink compose --embed {name}")
    elif entry["kind"] == "builtin":
        lines.append(f'<span class="c"># statically linked when embedded into libduckdb</span>')
        lines.append(f'EMBED_EXTENSIONS="{name}" ./scripts/build-libduckdb-wasm.sh')
        lines.append("")
        lines.append(f'<span class="c"># then it is available with no LOAD</span>')
    else:  # official
        lines.append(f'<span class="c"># build it into the core (needs its native deps)</span>')
        lines.append(f"ducklink compose --embed {name}")
    return "<br>".join(lines)


def render_detail(entry: dict, style: str, template: str, assets: dict) -> str:
    name = entry["name"]
    has_asset = artifact_name(entry) in assets if artifact_name(entry) else False
    exports = "".join(
        f'<span class="export">{esc(e)}</span>' for e in entry.get("exports", [])
    ) or '<span class="desc">— no individually-listed functions —</span>'

    kv = []
    def row(k, v):
        if v:
            kv.append(f"<dt>{esc(k)}</dt><dd>{v}</dd>")
    row("Kind", esc(entry["kind"]))
    row("Status", esc(entry["status"]))
    row("Version", esc(entry.get("version")))
    row("Categories", ", ".join(esc(c) for c in entry.get("categories", [])))
    if entry.get("repository"):
        r = esc(entry["repository"])
        row("Repository", f'<a href="{r}">{r}</a>')
    if has_asset:
        stem = artifact_name(entry)
        a = assets[stem]
        row("Artifact", f'<a href="/asset/{esc(stem)}">{esc(stem)}.wasm</a> '
                        f'({a["size_bytes"]:,} bytes)')
        row("SHA-256", f'<code>{esc(a["sha256"][:16])}…</code>')

    # The template has `class="badge {{STATUS}}">{{STATUS}}</span>`: the first
    # {{STATUS}} is the css class (normalized), the second is the display text.
    out = template.replace('class="badge {{STATUS}}"',
                           f'class="badge {badge_class(entry["status"])}"')
    repl = {
        "{{STYLE}}": style,
        "{{NAME}}": esc(name),
        "{{DESC}}": esc(entry.get("description", "")),
        "{{DESC_ATTR}}": esc((entry.get("description") or "")[:160]),
        "{{STATUS}}": esc(entry["status"]),
        "{{KIND}}": esc(entry["kind"]),
        "{{EXPORTS}}": exports,
        "{{KV}}": "".join(kv),
        "{{INSTALL}}": render_install(entry, has_asset),
    }
    for k, v in repl.items():
        out = out.replace(k, v)
    return out


def render_landing(entries: list[dict], style: str, template: str, meta: dict) -> str:
    search = [{
        "name": e["name"], "kind": e["kind"], "status": e["status"],
        "description": e.get("description", ""), "exports": e.get("exports", []),
        "categories": e.get("categories", []),
    } for e in entries]
    fn_count = sum(len(e.get("exports", [])) for e in entries)
    reg = meta.get("registry_url", "")
    reg_short = reg.replace("https://", "").replace("http://", "")
    out = template
    for k, v in {
        "{{STYLE}}": style,
        "{{TOTAL_COUNT}}": str(len(entries)),
        "{{FN_COUNT}}": str(fn_count),
        "{{REGISTRY_URL}}": esc(reg),
        "{{REGISTRY_URL_SHORT}}": esc(reg_short),
        "{{DATA_JSON}}": json.dumps(search, separators=(",", ":")),
    }.items():
        out = out.replace(k, v)
    return out


def api_one(entry: dict, assets: dict) -> dict:
    d = {k: entry[k] for k in ("name", "kind", "status", "description",
                               "exports", "categories", "version",
                               "repository", "source")}
    stem = artifact_name(entry)
    if stem and stem in assets:
        a = assets[stem]
        d["artifact"] = {
            "url": f"/asset/{stem}", "size_bytes": a["size_bytes"],
            "sha256": a["sha256"], "content_type": a["content_type"],
        }
    return d


# ---------------------------------------------------------------------------
# Routes (DuckDB SQL handlers; $path bound by ducklink serve).
# ---------------------------------------------------------------------------
EXT_HANDLER = (
    "SELECT coalesce((SELECT html FROM pages WHERE name = regexp_replace($path, '^/ext/', '')), "
    "'<!doctype html><meta charset=utf-8><title>404</title>"
    "<body style=\"font-family:sans-serif;padding:48px\"><h1>404</h1>"
    "<p>No such extension. <a href=\"/\">Back to the registry</a>.</p>') AS body, "
    "CASE WHEN EXISTS(SELECT 1 FROM pages WHERE name = regexp_replace($path, '^/ext/', '')) "
    "THEN 200 ELSE 404 END AS status"
)
API_ONE_HANDLER = (
    "SELECT coalesce((SELECT json FROM api_ext WHERE name = "
    "regexp_replace(regexp_replace($path, '^/api/ext/', ''), '\\.json$', '')), "
    "'{\"error\":\"not found\"}') AS body, "
    "CASE WHEN EXISTS(SELECT 1 FROM api_ext WHERE name = "
    "regexp_replace(regexp_replace($path, '^/api/ext/', ''), '\\.json$', '')) "
    "THEN 200 ELSE 404 END AS status"
)
ASSET_HANDLER = "SELECT blob FROM assets WHERE name = regexp_replace($path, '^/asset/', '')"
ASSET_INFO_HANDLER = (
    "SELECT info_json AS body FROM assets WHERE name = regexp_replace($path, '^/asset-info/', '')"
)
STATIC_LOGO_HANDLER = "SELECT blob FROM static_assets WHERE name = 'ducklink_logo.png'"


def main() -> int:
    style = (TEMPLATES / "style.css").read_text()
    index_tpl = (TEMPLATES / "index.html").read_text()
    ext_tpl = (TEMPLATES / "ext.html").read_text()

    entries, meta = load_entries()
    assets = collect_assets()

    landing = render_landing(entries, style, index_tpl, meta)
    api_all = json.dumps([api_one(e, assets) for e in entries], indent=2)
    pages = {e["name"]: render_detail(e, style, ext_tpl, assets) for e in entries}
    api_pages = {e["name"]: json.dumps(api_one(e, assets), indent=2) for e in entries}

    if OUT.exists():
        OUT.unlink()
    con = duckdb.connect(str(OUT))
    con.execute("""
        CREATE TABLE routes (
            method VARCHAR NOT NULL, pattern VARCHAR NOT NULL, handler VARCHAR NOT NULL,
            kind VARCHAR NOT NULL DEFAULT 'sql', status INTEGER DEFAULT 200,
            ctype VARCHAR, priority INTEGER DEFAULT 0);
        CREATE TABLE pages (name VARCHAR PRIMARY KEY, html VARCHAR NOT NULL);
        CREATE TABLE api_ext (name VARCHAR PRIMARY KEY, json VARCHAR NOT NULL);
        CREATE TABLE extensions (
            name VARCHAR PRIMARY KEY, kind VARCHAR, status VARCHAR, description VARCHAR,
            version VARCHAR, repository VARCHAR, categories_json VARCHAR, exports_json VARCHAR);
        CREATE TABLE assets (
            name VARCHAR PRIMARY KEY, blob BLOB NOT NULL, size_bytes INTEGER NOT NULL,
            sha256 VARCHAR NOT NULL, content_type VARCHAR NOT NULL DEFAULT 'application/wasm',
            info_json VARCHAR);
        CREATE TABLE static_assets (name VARCHAR PRIMARY KEY, blob BLOB NOT NULL, content_type VARCHAR);
        CREATE TABLE site_meta (key VARCHAR PRIMARY KEY, value VARCHAR);
    """)

    con.executemany("INSERT INTO pages VALUES (?, ?)", list(pages.items()))
    con.executemany("INSERT INTO api_ext VALUES (?, ?)", list(api_pages.items()))
    con.executemany(
        "INSERT INTO extensions VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        [(e["name"], e["kind"], e["status"], e.get("description"), e.get("version"),
          e.get("repository"), json.dumps(e.get("categories", [])),
          json.dumps(e.get("exports", []))) for e in entries],
    )
    for a in assets.values():
        info = json.dumps({
            "name": a["name"], "size_bytes": a["size_bytes"], "sha256": a["sha256"],
            "content_type": a["content_type"], "url": f"/asset/{a['name']}",
        })
        con.execute("INSERT INTO assets VALUES (?, ?, ?, ?, ?, ?)",
                    [a["name"], a["blob"], a["size_bytes"], a["sha256"], a["content_type"], info])

    if LOGO.is_file():
        con.execute("INSERT INTO static_assets VALUES (?, ?, ?)",
                    ["ducklink_logo.png", LOGO.read_bytes(), "image/png"])
    else:
        print(f"WARNING: logo not found at {LOGO}", file=sys.stderr)

    con.executemany("INSERT INTO site_meta VALUES (?, ?)", [
        ("extensions", str(len(entries))),
        ("functions", str(sum(len(e.get("exports", [])) for e in entries))),
        ("assets", str(len(assets))),
    ])

    rows = [
        ("GET", "/health", "ok", "static", 200, "text/plain; charset=utf-8", 0),
        ("GET", "/", landing, "static", 200, "text/html; charset=utf-8", 0),
        ("GET", "/api/extensions.json", api_all, "static", 200, "application/json", 0),
        ("GET", "/ext/*", EXT_HANDLER, "sql", 200, "text/html; charset=utf-8", 0),
        ("GET", "/api/ext/*", API_ONE_HANDLER, "sql", 200, "application/json", 0),
        ("GET", "/asset/*", ASSET_HANDLER, "blob", 200, "application/wasm", 0),
        ("GET", "/asset-info/*", ASSET_INFO_HANDLER, "sql", 200, "application/json", 0),
        ("GET", "/static/ducklink_logo.png", STATIC_LOGO_HANDLER, "blob", 200, "image/png", 10),
        ("GET", "/favicon.ico", STATIC_LOGO_HANDLER, "blob", 200, "image/png", 10),
    ]
    con.executemany(
        "INSERT INTO routes (method, pattern, handler, kind, status, ctype, priority) "
        "VALUES (?, ?, ?, ?, ?, ?, ?)", rows)
    con.close()

    size = OUT.stat().st_size
    print(f"wrote {OUT}  —  {len(entries)} extensions, {len(assets)} artifacts, {size:,} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
