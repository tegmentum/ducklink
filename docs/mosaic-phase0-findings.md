# Mosaic on DuckLink — Phase 0 findings

Purpose: de-risk the transport path for the future `mosaic-component`
DuckDB extension **before** we build anything. This memo records what
`ducklink serve` gives us today, what Mosaic's REST connector expects
on the wire, what the Phase 0 spike proved, and what Phase 1 has to do.

- Sibling: [Phase 0 spike](./mosaic-phase0-spike/) — throwaway shell +
  HTML that proves the full round-trip; run with
  `bash docs/mosaic-phase0-spike/run.sh`.

## 1. Server infrastructure — how `ducklink serve` works today

### 1.1 The routes table

- **DDL owner**: the server itself, gated on `--init-routes`. Created
  in `init_routes_table` at
  `crates/ducklink-host/src/httpd.rs:949-982` with columns
  `(method, pattern, handler, kind, status, ctype, priority)`, kind
  defaulting to `'sql'`. If `--init-routes` is passed and the table is
  empty, the server also seeds one `GET /hello` demo route.
- **Bootstrap options**:
  1. Operator flag `--init-routes` on `ducklink serve`
     (`crates/ducklink-host/src/bin/ducklink.rs:759`).
  2. Operator SQL: `POST /sql` with the DDL from
     `docs/duckdb-wasm-httpd.md:47-57`.
  3. Extension SQL via `nested-exec` (see §1.2). **No extension does
     this today** — a repo-wide grep for `INSERT INTO routes` in
     `extensions/*/src/**` finds zero hits.
- **Routing lookup**: `lookup()` at `httpd.rs:442-494` runs
  `WHERE (method = $1 OR method = '*') AND $2 GLOB pattern
  ORDER BY priority DESC, length(pattern) DESC LIMIT 1`. Higher-priority
  rows win; `mosaic.create` should use `priority >= 10` so extension
  routes beat any operator fallbacks.

### 1.2 Can an extension INSERT into `routes` from inside a running server?

**Yes, via `nested-exec`.** Contract at
`wit/duckdb-extension/nested-exec.wit:1-44`: the host runs the SQL on a
**sibling connection to the same underlying database**, so DDL / DML is
visible to the accept-loop's connection immediately (a fresh autocommit
transaction commits before returning). Two consequences:

- A `mosaic.create('name', $$json$$)` scalar can execute
  `INSERT INTO routes ...` inside its callback. The next HTTP request
  the httpd receives will see the row.
- Because it's a sibling connection, `mosaic.create` will not see the
  outer transaction's uncommitted state. Users who call it inside a
  `BEGIN` should be told that the route lands on commit, not before.

Nesting depth is capped (default 4) — plenty for `INSERT INTO routes`.

### 1.3 SQL-kind handler response policy

- Content-Type: comes from `routes.ctype` (defaults to
  `application/json` at `httpd.rs:711,745,768`).
- Structured mode: a handler returning `body`/`status`/`ctype`
  columns has those override the row (`build_route_response`,
  `httpd.rs:704-771`). This is the mechanism for **returning raw bytes
  as any content-type** — including future Arrow IPC:
  `SELECT arrow_bytes AS body, 'application/vnd.apache.arrow.stream' AS ctype`
  is already supported by the server; the missing piece is a wasm-core
  SQL function that emits Arrow IPC bytes (see §3).
- The `body` value is coerced via `dv_to_body_bytes` at
  `httpd.rs:860-890`. `BLOB` values pass through **verbatim** — so the
  Arrow path works the moment a suitable SQL function exists.
- CORS: `Access-Control-Allow-Origin: *` on every response
  (`httpd.rs:273`). Good for the browser-on-same-origin case; the same
  header lets an operator later host the SPA on a different origin.

### 1.4 Static-kind handler

- `handler` column **is** the response body, verbatim (bytes-in-database).
  No ETag, no gzip, no content-hashing — see `execute_route` at
  `httpd.rs:505-508`. Fine for a small `index.html` + one small JS
  bundle; a real SPA with a MB of vendor JS would want either the
  `blob`-kind route (which streams from a table cell — still no ETag)
  or a Phase-2 upgrade to the static handler that adds
  `ETag`/`If-None-Match` and gzip.

### 1.5 wasm-kind handler (out of scope for Phase 0)

Fully implemented already — not a 501. `execute_wasm` at
`httpd.rs:518-536` dispatches to a `HandlerRegistry` populated via
`--load NAME=PATH`. Components implement the
`duckdb:handler/request-handler` world exporting
`handler.handle(request-json) -> result<string, string>`. If no
handlers are loaded, matching a `kind='wasm'` route returns a plain
501 explaining `--load` is required
(`httpd.rs:524-529`).

Practical consequence for Mosaic: we **don't need** to touch this
handler for Phase 1. `sql` + `static` (and later `blob`) cover the
whole surface. `wasm`-kind is only interesting if we want the Mosaic
extension to intercept requests directly rather than letting DuckDB
run the SQL — which we don't.

## 2. Mosaic protocol — what the browser expects

Fetched from `uwdata/mosaic` at
`packages/mosaic/core/src/connectors/{Connector.ts,rest.ts}` on
`main` (retrieved via `gh api ...contents/... base64 -d`):

- **Connector interface**: one method,
  `query(req) -> Promise<Table | Record<string,unknown>[] | void>`.
- **REST wire format**: `POST <uri>` with
  `Content-Type: application/json`, body
  `{ "type": "arrow" | "json" | "exec", "sql": "..." }`.
- **Server response** (branch on request's `type`):
  - `arrow` → **raw Apache Arrow IPC bytes** (the body is fed to
    `flechette.decodeIPC(arrayBuffer)`).
  - `json`  → **JSON array of row objects**
    (`Record<string, unknown>[]`).
  - `exec`  → any body, ignored (the Promise just resolves).
- The reference server side lives at
  `packages/server/duckdb-server-rust/src/query.rs` and does exactly
  this dispatch on top of `duckdb::get_arrow(sql) -> Vec<u8>` /
  `get_json(sql) -> Vec<u8>`.

**Implication for Phase 1**: the DuckLink connector we ship in the
extension's SPA can be **the standard `restConnector`**, provided our
`/api/query` route implements this JSON-envelope contract. See §5.

Browser packages needed for a minimum vgplot demo (from
`packages/vgplot/vgplot/package.json`):
`@uwdata/mosaic-core`, `@uwdata/mosaic-sql`, `@uwdata/mosaic-inputs`,
`@uwdata/mosaic-plot`, `@uwdata/vgplot` (all under one
`@uwdata/vgplot` re-export). Runtime deps that must be bundled with
them: `@uwdata/flechette` (Arrow decoder), `@observablehq/plot`, `d3`.

## 3. Arrow round-trip — what we have and what we're missing

- The httpd is **transport-ready** for Arrow: BLOB `body` columns pass
  through verbatim, and `ctype` is caller-controlled (§1.3).
- **What's missing**: a wasm-core SQL function that returns
  Arrow IPC bytes for a subquery result. The upstream reference server
  calls DuckDB's C++ `Connection::Query(...)` + `ArrowIPCReader`; our
  WIT surface exposes `execute(prepared) -> {columns, rows}` rows only.
- **Phase 0 verdict**: JSON is fine. Mosaic explicitly supports
  `type: "json"` — many Mosaic samples run against Arrow for
  performance, but the protocol is fully JSON-capable. We will ship
  JSON in Phase 1 and add Arrow in Phase 2 when the wasm core (or a
  helper extension) exposes an `arrow_ipc(query)` SQL function.

## 4. What the spike proved

The spike at `docs/mosaic-phase0-spike/` is a shell script + one
hand-written `index.html`, no build step. Running:

```
bash docs/mosaic-phase0-spike/run.sh
open http://localhost:8787/
```

verbatim shell output during a live run:

```
--- GET / (first 200 bytes) ---
<!doctype html>
<meta charset="utf-8">
<title>Mosaic Phase 0 spike</title>
...

--- POST /api/query ---
[{"ts":"2026-01-01 00:00:00","value":30},
 {"ts":"2026-01-02 00:00:00","value":34.79425538604203},
 {"ts":"2026-01-03 00:00:00","value":38.414709848078964},
 {"ts":"2026-01-04 00:00:00","value":39.97494986604055},
 {"ts":"2026-01-05 00:00:00","value":39.09297426825682}]

--- content-types ---
Content-Type: application/json
```

The browser page loads `@observablehq/plot` from a CDN, issues one
`fetch('/api/query', {method:'POST', body: SQL})`, and renders a
line-plus-dot chart of a synthetic sin-wave time series plus a
20-row HTML table. Every hop worked on the first try:

1. `ducklink serve --db spike.duckdb --port ... --init-routes` starts.
2. `POST /sql CREATE TABLE fixture ...` seeds the DB (returns
   `{"columns":["Count"],"rows":[[20]],...}`).
3. `INSERT INTO routes` for the static `/` and `POST /api/query` sql
   route land immediately — the next request sees them.
4. `POST /api/query` with `SELECT ...` in the body runs via the
   `SELECT * FROM query($body)` handler and returns a JSON array of
   row objects with `Content-Type: application/json`.
5. The browser parses the JSON and Plot renders.

### What did NOT work

- `--db /absolute/path/to/spike.duckdb` fails: the wasi-fs shim
  normalises to the cwd preopen and DuckDB then tries to open the
  file at the wasi-relative path, which doesn't exist. Workaround:
  `cd`  to the DB's dir and pass a plain relative filename. The spike
  does this (`WORK_DIR=$(mktemp -d)` + `--db spike.duckdb`). This is a
  **DuckLink papercut worth filing** — either the resolver should
  accept absolute host paths that fall inside the cwd preopen, or
  `ducklink serve` should document that `--db` is cwd-relative.

## 5. Top findings that shape Phase 1

1. **We can use Mosaic's stock `restConnector` unchanged.** Once our
   `/api/query` route implements the `{type, sql}` envelope and
   returns JSON arrays / raw Arrow bytes on the right branch, there is
   nothing bespoke on the browser side. That kills Option A (native
   protocol) and Option B (custom `DuckLinkConnector`) in favour of
   **Option C: implement the standard REST-connector contract on the
   server side and let mosaic-core talk to us with zero client code.**
2. **Route registration from inside an extension works.** `nested-exec`
   inserts land atomically on the sibling connection and are visible
   to the httpd's next request. No new WIT interface required.
3. **The transport is already Arrow-ready; the SQL-side isn't yet.**
   The httpd will happily emit `BLOB` bodies with any content-type;
   what's missing is a SQL function that produces Arrow IPC bytes.
   Phase 1 goes out on JSON; Phase 2 upgrades once we have
   `arrow_ipc(query) -> BLOB` in the wasm core (or ship it as a
   helper extension).
4. **We don't need to touch `httpd.rs`.** All four handler kinds we
   need — `sql` for `/api/query`, `static` for the SPA shell,
   `blob` for larger assets, and `sql` again for the spec endpoint —
   are present today. That contains Phase 1 to the extension only.
5. **Papercut**: `--db PATH` is silently cwd-relative. Not blocking
   Phase 1 (the extension author will typically start ducklink in the
   project directory), but worth a follow-up ticket separate from
   Mosaic.

## 6. Phase 1 build plan — `mosaic-component`

Ordered tasks. Sizes: **S** ~half-day, **M** 1-2 days, **L** 3+ days.

1. **Scaffold `extensions/mosaic-component/`** (S). Copy the layout
   of `extensions/cache-component/` (uses `nested-exec` + a scalar
   surface). Wire `wit/duckdb-extension.wit` + `wit/nested-exec.wit`.
2. **Design the routes-schema addition** (S). Decide on a companion
   `mosaic_apps(name, spec_json, created_at)` table so `mosaic.list()`
   and `mosaic.drop(name)` can enumerate and unregister without
   parsing the routes table. Extension owns its DDL, guarded by
   `CREATE TABLE IF NOT EXISTS`.
3. **Implement `mosaic.create(name TEXT, spec JSON) -> TEXT`** (M).
   Runs three nested-exec statements:
   - `INSERT INTO mosaic_apps ...` (upsert on name).
   - `INSERT INTO routes` for `GET /mosaic/app/{name}` kind=static —
     the SPA shell HTML (see task 6).
   - `INSERT INTO routes` for `GET /mosaic/spec/{name}` kind=sql
     returning the spec JSON.
   - `INSERT INTO routes` for `POST /mosaic/query` kind=sql (see
     task 4). One row shared across all apps.
   Returns the app URL. Uses `priority >= 10`.
4. **The `/mosaic/query` sql-kind handler** (M). Must implement the
   `restConnector` envelope. Sketch:
   ```sql
   SELECT
     CASE
       WHEN json_extract_string($body, '$.type') = 'json'
         THEN (SELECT to_json(list(row))
               FROM query(json_extract_string($body, '$.sql')) row)
       WHEN json_extract_string($body, '$.type') = 'exec'
         THEN (SELECT query_exec(json_extract_string($body, '$.sql')))
       ELSE json_extract_string($body, '$.sql')  -- unsupported type
     END AS body,
     'application/json' AS ctype,
     200 AS status
   ```
   Details to work out: (a) whether `to_json(list(row))` produces the
   `[{col:val,...},...]` shape mosaic expects, or if we need a
   `json_group_array(row_to_object(...))` variant; (b) exec branch —
   `query()` is SELECT-only, so exec needs a helper macro that runs
   `PRAGMA` / DDL. This SQL likely wants to live in a
   `CREATE MACRO` created by the extension at load time and the route
   just calls the macro.
5. **`mosaic.drop(name)` / `mosaic.list()` scalars** (S).
   Straightforward DELETE / SELECT via nested-exec.
6. **Ship the SPA shell** (M).
   - Contents: one `index.html` that imports the vgplot bundle,
     configures `coordinator.databaseConnector(restConnector({uri:
     '/mosaic/query'}))`, fetches `/mosaic/spec/{name}` and calls
     `vg.parseSpec(spec)`.
   - Build: `pnpm` monorepo `web/mosaic-shell/` that produces a
     single `dist/index.html` + `dist/mosaic.js` with everything
     inlined (esbuild + `--bundle --format=esm --minify`).
   - **Distribution question — needs user input, see §7**: does the
     bundled JS get embedded in the wasm component (like the current
     `ui_console.html` in `ui_server.rs`) and served via a `blob`-kind
     route out of a `.duckdb_files`-style table? Or staged into the
     component's on-disk assets and copied out at extension-install
     time? The former keeps `.duckdb` files self-contained and travels
     with the component; the latter avoids a rebuild every time we
     bump `@uwdata/vgplot`. My recommendation: **embed** — Mosaic
     upgrades are rare and the whole point of "extension = single wasm
     component" is portability. Bundle size to check: rough guess
     ~500 KB gzipped for vgplot + plot + arrow + flechette.
7. **Smoke test** (S). `extensions/mosaic-component/smoke.sql` —
   `LOAD mosaic; SELECT mosaic.create('demo', $${...simple spec...}$$);`
   then a curl to `/mosaic/query` with a canned `{type:'json',sql:...}`
   body. Compare against `smoke.expected` in the standard way.
8. **Catalog entry** (S). Register `mosaic-component` in the
   workspace catalog with a smoke-comment; digest-version the WIT.

**No WIT changes required.** We reuse `nested-exec` (§1.2) and the
existing scalar-registry contract. If task 4 finds `to_json` doesn't
produce the shape we want, we could pull in `runtime.scalar-registry`
to register a `mosaic_json_rows()` scalar as a workaround — that's an
S-sized fallback, not a WIT change.

## 7. Open questions for the user before Phase 1

1. **JS bundle distribution** (§6, task 6): embed in the wasm
   component or stage on disk under `artifacts/`? Recommendation:
   embed. Confirm or change.
2. **Naming**: is the app URL layout `/mosaic/app/{name}` +
   `/mosaic/query` OK, or should it live under `/ducklink/mosaic/...`
   per the original design memo (which prefixed everything with
   `/ducklink/`)? I dropped the prefix in the plan above because
   `ducklink serve` doesn't otherwise namespace its routes, but the
   memo's convention is fine too.
3. **Auth**: any need for a token/header check on `/mosaic/query` in
   Phase 1, or is "same-origin + localhost bind is the security model"
   acceptable for now? (Same-origin is what the spike assumes.)
4. **Spec input format**: does `mosaic.create` accept a Mosaic vgplot
   **YAML** spec (Mosaic's canonical author format) or **JSON**
   (already a first-class DuckDB type)? YAML would need a translator
   scalar; JSON is trivial. Recommendation: JSON only in Phase 1, add
   YAML in Phase 2 if the DX matters.
5. The `--db PATH` cwd-relative papercut (§4): file as a separate
   issue, or handle in Phase 1 to smooth the mosaic bootstrap
   experience?
