# ducklink:mosaic

**Phase 1 of the in-database Mosaic app engine** — per
`docs/mosaic-phase0-findings.md` + the accepted design memo. Loads as a
standard `duckdb:extension` wasm component; imports
`duckdb:extension/nested-exec@5.0.0`; ships an embedded browser bundle
(mosaic-spec + vgplot + Observable Plot) so a `.duckdb` file is a
self-contained Mosaic distribution.

## SQL surface (shipped)

| Function | Notes |
| --- | --- |
| `mosaic_create(name, spec_json)` -> TEXT | Aspirational API — currently blocked by the nested-exec re-entry issue documented below. |
| `mosaic_create(name, spec_json, opts_json)` -> TEXT | Same, arity-3 overload. |
| `mosaic_drop(name)` -> BOOL | Same blocker. |
| `mosaic_url(name)` -> TEXT | Same blocker (reads `__mosaic_apps`). |
| `mosaic_spec(name)` -> TEXT | Same blocker. |
| `mosaic_plot(sql, kind, opts_json)` -> TEXT | Same blocker (calls `mosaic_create` internally). |
| **`mosaic_plot_spec(sql, kind, opts_json)`** -> TEXT | **Pure** — returns the canonical vgplot spec JSON. No nested-exec. |
| **`mosaic_install_sql(name, spec_json, opts_json)`** -> TEXT | **Pure** — returns the semicolon-batched SQL an operator can `POST /sql` back to install an app without needing nested-exec re-entry. This is the Phase 1 workaround the E2E script uses; see below. |

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

## Known limitation — nested-exec re-entry

`mosaic_create` internally issues `INSERT INTO routes` + `INSERT INTO
__mosaic_apps` via the `duckdb:extension/nested-exec@5.0.0` host
import. Against the current standalone-ducklink CLI this traps with:

```
Invalid Input Error: invalid argument: nested-exec: primary
call_execute trapped: wasm trap: cannot enter component instance
```

The same trap affects `fieldbook_create` today — the primary-store
re-entry path (commit `d06c7d7`) landed but is blocked pending an
ecosystem-wide core-wasm rebuild (see the commit body). Not fixed
here because the constraint is _do not touch
`crates/ducklink-{host,runtime,cli}`_.

**Workaround shipped in Phase 1**: `mosaic_install_sql(...)` returns the
exact SQL `mosaic_create` would execute internally. Callers `POST /sql`
that string back to a running `ducklink serve` (whose httpd connection
is outside the scalar-callback stack, so the same INSERTs succeed
there). `scripts/mosaic-phase1-e2e.sh` demonstrates the full flow end
to end. Once the trap is lifted, `mosaic_create` starts working with
zero code changes here.

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

* `python3 tooling/smoke.py mosaic` — pure-surface smoke (URL / spec /
  install-SQL builders + the vgplot spec generator).
* `bash scripts/mosaic-phase1-e2e.sh` — full round-trip: starts
  `ducklink serve`, builds install SQL via `mosaic_install_sql`, POSTs
  it, curls every route, asserts the auth branches. Prints an
  openable URL at the end (`KEEP_ALIVE=1` leaves the server up).

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
