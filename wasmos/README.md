# Consuming DuckLink from the Tegmentum wasmos stack

This directory holds documentation, not code. The Rust host + wasip2
component live in `../crates/` and `../wit/`; the wasmos-side JS
adapter that lets a browser app use `ducklink_core.wasm` as a
`wasmos:dataset` provider lives in the sibling elena-wasm workspace.

## Where the pieces live

| Concern | Package / crate | Location |
|---|---|---|
| WIT contract (`duckdb:component/database`) | this repo | `wit/core/wit/duckdb-core.wit` |
| wasip2 core component | this repo | `crates/ducklink-core/` → `ducklink_core.wasm` |
| Native host (`wasmtime`) | this repo | `crates/ducklink-host/` (`ducklink` CLI binary) |
| Browser boot (this repo's demo) | this repo | `web/` (see `web/README.md`) |
| Portable dataset ABI | elena-wasm | `packages/dataset/` (`@wasmos/dataset`) |
| Browser adapter → `Dataset` | elena-wasm | `packages/dataset-ducklink/` (`@wasmos/dataset-ducklink`) |
| WIT-typed dashboard demo | elena-wasm | `examples/vela-dashboard/` |

## What the elena-wasm-side `@wasmos/dataset-ducklink` gives you

The elena-wasm workspace ships a package whose only job is to boot
`ducklink_core.wasm` in the browser and adapt
`duckdb:component/database` to elena-wasm's `wasmos:dataset`
interface — the D-018 portable dataset ABI that
[`@wasmos/dataset`](https://github.com/tegmentum/elena-wasm/tree/main/packages/dataset)
defines. From the consuming app:

```js
import { createDuckLinkDataset } from "@wasmos/dataset-ducklink";

const dataset = await createDuckLinkDataset({
    corePath: "/ducklink_core.wasm",
    sqlSeed: [
        "CREATE TABLE sales (ts TIMESTAMP, region VARCHAR, amount INTEGER)",
        "INSERT INTO sales VALUES ('2026-08-09 12:00:00', 'us-west', 100)",
    ],
});

const snapshot = await dataset.query({
    sql: "SELECT region, SUM(amount) AS revenue FROM sales GROUP BY region",
    parameters: { tag: "positional", val: [] },
});
```

The same `Dataset` shape (`query` / `subscribe` / `apply`) that any
other `wasmos:dataset` provider implements — the DuckDB-Wasm-based
`@wasmos/dataset-duckdb-wasm`, the mock provider that drives SSR,
future adapters against Postgres / DuckDB-server / whatever. D-019's
"WIT is the contract; wasm is one implementation strategy" thesis in
production: the `ducklink_core.wasm` binary runs unchanged native
under wasmtime as `ducklink_cli`, and in-browser via
`@tegmentum/wasi-polyfill`.

See the full API + parameter-binding semantics in
[`packages/dataset-ducklink/README.md`](https://github.com/tegmentum/elena-wasm/tree/main/packages/dataset-ducklink/README.md).

## How the browser boot works

Both the elena-wasm-side `@wasmos/dataset-ducklink` and this repo's
own `web/` demo share the same boot shape (see `web/run-core.mjs`):

1. `fetch()` the `ducklink_core.wasm` component bytes.
2. Configure `@tegmentum/wasi-polyfill` with the plugin set the core
   imports (cli / io / filesystem / clocks / random / sockets) plus a
   writable in-memory `/` preopen and a pre-created `/.duckdb`
   directory (DuckDB's non-recursive `CreateDirectory` succeeds on
   `LOAD`).
3. Instantiate the component via wasi-polyfill's `RuntimeBindgen`
   with JSPI async promoted on the poll import + the execute export
   (Chrome 137+ / Playwright's bundled Chromium).
4. Open an in-memory `duckdb:component/database.connection`, run
   any `sqlSeed` statements.
5. Return a WIT-typed handle whose SQL verbs lower to the WIT
   `execute` / `prepare(...).execute(...)` calls.

## Shared toolchain: `wit-js-bindgen`

Both consumers regenerate their JS bindings from this repo's WIT tree
with the same [`wit-js-bindgen`](https://github.com/tegmentum/wit-js-bindgen)
invocation:

```bash
wit-js-bindgen wit/core/wit \
    --world libduckdb \
    --role consumer \
    --auto-alias-wasi \
    --out <dest>
```

- **elena-wasm's `just gen-ducklink`** points at
  `${DUCKLINK_DIR}/wit/core/wit` (default `~/git/ducklink/wit/core/wit`)
  and writes to `packages/dataset-ducklink/src/bindings/`.
- **This repo's `web/generate.sh`** (or `npm run generate` under
  `web/`) points at `wit/core/wit` and writes to `web/bindings/`.

The generated output is byte-identical when both consumers run against
the same DuckLink checkout — the tool is deterministic and the WIT
tree is the single source of truth. See
[the wit-js-bindgen README](https://github.com/tegmentum/wit-js-bindgen/blob/main/README.md)
for the emitted shape (canonical-ABI marshal, WIT-derived types,
`instantiate(module, imports)` factory).

## Consuming ducklink_core.wasm as a build-time asset

The wasm binary is the build-time dependency; the JS bindings + the
adapter code fall out of it. Two paths:

1. **Sibling checkout (recommended for local dev).** Clone this repo
   next to elena-wasm and build the core:

   ```bash
   cd ~/git/ducklink && make core-browser
   ```

   The elena-wasm dashboard demo's build script
   (`examples/vela-dashboard/build-dashboard.mjs`) copies the built
   wasm from `~/git/ducklink/web/public/` (or the
   `target/wasm32-wasip2/release/` output) into
   `examples/vela-dashboard/public/`. Overridable via
   `DUCKLINK_DIR=/other/path`.

2. **Prebuilt binary distribution.** A future landing may publish
   `ducklink_core.wasm` as an npm asset or a CDN download; today the
   sibling checkout is the path.

## Rust-side integration

Consumers hosting DuckLink outside the browser (server-side, native
`wasmtime`, embedded, ...) go through the `ducklink-host` crate + the
`duckdb:component/database` WIT interface directly — no browser
transpile chain. See the top-level `README.md`'s "Native host runner"
section.

A dedicated `wasmos-dataset-ducklink` Rust crate (implementing
elena-wasm's `wasmos-dataset-duckdb` trait against a DuckLink-hosted
DuckDB) is a follow-up landing tracked against elena-wasm's PLAN. For
now, hosts that need `wasmos:dataset` semantics on the server go
through elena-wasm's own [`crates/wasmos-dataset-duckdb`](https://github.com/tegmentum/elena-wasm/tree/main/crates/wasmos-dataset-duckdb)
using a native `libduckdb`, and reserve the DuckLink wasip2 boot for
the browser lane. The two implementations satisfy the same D-018
`Dataset` ABI; picking a lane is a deployment choice.

## Cross-references

- Elena-wasm's decisions:
  [D-018](https://github.com/tegmentum/elena-wasm/blob/main/docs/decisions.md#d-018)
  (WIT-portable dataset ABI),
  [D-019](https://github.com/tegmentum/elena-wasm/blob/main/docs/decisions.md#d-019)
  (WIT is the contract; wasm is one implementation strategy).
- Elena-wasm's `PLAN.md §14.21` — the shipped
  `@wasmos/dataset-ducklink` landing note (also captures the
  `exception-refs` blocker described in `web/README.md`).
- `wit-js-bindgen` — the WIT-to-JS/TS binding generator both
  consumers share.
