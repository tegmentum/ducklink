# ducklink:mosaic

**Phase 1 of the in-database Mosaic app engine** — per
`docs/mosaic-phase0-findings.md` + the accepted design memo. Loads as a
standard `duckdb:extension` wasm component; imports
`duckdb:extension/nested-exec@5.0.0`; ships an embedded browser bundle
(mosaic-spec + vgplot + Observable Plot) so a `.duckdb` file is a
self-contained Mosaic distribution.

## SQL surface

| Function | Notes |
| --- | --- |
| `mosaic_create(name, spec_json)` -> TEXT | Installs an app + its routes; returns the app URL. Uses `nested-exec` to write `__mosaic_apps` + `routes`. |
| `mosaic_create(name, spec_json, opts_json)` -> TEXT | Same, arity-3 overload. |
| `mosaic_drop(name)` -> BOOL | Removes the app + its routes. |
| `mosaic_url(name)` -> TEXT | Returns the installed app URL (with embedded token, if any). |
| `mosaic_spec(name)` -> TEXT | Returns the stored vgplot spec JSON. |
| `mosaic_plot(sql, kind, opts_json)` -> TEXT | Convenience: builds a plot spec + calls `mosaic_create` internally; returns the URL. |
| `mosaic_plot_spec(sql, kind, opts_json)` -> TEXT | **Pure** — returns the canonical vgplot spec JSON. No nested-exec. |

`opts_json` (all optional):

```json
{
  "token":       "auto" | "none" | "<hex-string>",   // default "auto"
  "description": "..."
}
```

## URL layout

```
/ducklink/mosaic/app/{name}                    -- SPA index.html   (static)
/ducklink/mosaic/app/{name}/bundle.js          -- JS runtime       (static)
/ducklink/mosaic/api/app/{name}/spec           -- spec JSON        (sql)
/ducklink/mosaic/api/app/{name}/query          -- Mosaic REST      (sql)
```

Per-app query routes (not a shared one) because DuckDB's `query()`
table function doesn't accept subqueries in its argument — see
`mosaic-core::build_app_query_route_sql` for the SQL shape.

## Prerequisites for `mosaic_create`

`mosaic_create` inserts into a `routes` table (the httpd router). The
table must exist before the first call — either run `ducklink serve
--init-routes` once, or `CREATE TABLE routes (...)` manually. The
end-to-end script (`scripts/mosaic-phase1-e2e.sh`) creates it inline.

Historical note: Phase 1 originally shipped a pure `mosaic_install_sql`
scalar as a workaround for a nested-exec re-entry trap at @4. That trap
was unblocked in the @5 host (ADR Decision 6 + shared-`ExtensionManager`
sibling connection), and the workaround scalar has been removed
upstream — `mosaic_create` is the only supported install path now.

## Building

```
make mosaic-browser     # esbuild bundles the ~800 KB browser runtime
make mosaic             # cargo component build + copy to artifacts/extensions/mosaic.wasm
```

`mosaic-browser` requires Node 20+ / `npm`. If those aren't available,
either pre-stage `extensions/mosaic-component/browser/dist/{bundle.js,
index.html}` by hand (a CDN-imports fallback along the Phase 0 spike
lines is one option) or use pre-built artifacts.

## Testing

* `python3 tooling/smoke.py mosaic` — pure-surface smoke: exercises the
  four supported `mosaic_plot_spec` mark kinds (line|bar|dot|area). No
  nested-exec, no filesystem — runs on the default `:memory:` harness.
* `bash scripts/mosaic-phase1-e2e.sh` — full round-trip: CLI seeds a
  fixture table + calls `mosaic_create` directly, then `ducklink serve`
  is started on the same DB and every route is curled (auth branches
  included). Prints an openable URL at the end (`KEEP_ALIVE=1` leaves
  the server up).

## Repo layout

```
extensions/mosaic-component/
├── Cargo.toml
├── README.md                       (this file)
├── smoke.sql + smoke.expected
├── src/lib.rs                      (thin shim; installs bridges into mosaic-core)
├── wit/                            (duckdb-extension-mosaic world)
└── browser/
    ├── package.json
    ├── esbuild.config.mjs
    ├── entry.js
    ├── index-template.html         (SPA shell; __NAME__ + __TOKEN__ placeholders)
    └── dist/{bundle.js,index.html} (esbuild output; embedded via include_bytes!)

# alongside, in the sibling datalink repo:
../../datalink/extensions/mosaic-core/
└── src/lib.rs                       (declare!-driven surface + all logic;
                                     no_std + alloc + std for serde_json)
```
