---
id: javascript
title: The JavaScript / TypeScript APIs
sidebar_label: JavaScript / TypeScript
---

# The JavaScript / TypeScript APIs

ducklink ships an ergonomic browser API on npm. Three packages cooperate:

| Package | Version | Role |
|---|---|---|
| [`@tegmentum/ducklink`](https://www.npmjs.com/package/@tegmentum/ducklink) | `0.4.0` | the DuckDB facade — `create` / `connect` / `query` / `load` + a typed `Result` |
| [`@tegmentum/sqlink`](https://www.npmjs.com/package/@tegmentum/sqlink) | `0.4.0` | the parallel SQLite facade (same shape, SQLite semantics) |
| [`@tegmentum/datalink-browser`](https://www.npmjs.com/package/@tegmentum/datalink-browser) | `0.4.0` | the shared low-level runtime both facades build on |

`@tegmentum/ducklink` and `@tegmentum/sqlink` are deliberately parallel facades;
`@tegmentum/datalink-browser` is the shared plumbing (WASI polyfill wiring,
component instantiation, the extension host, the catalog resolver), not an
ergonomic surface you call directly.

## The ducklink facade

The mental model is `create` → `connect` → `query`, with extensions loaded per
connection. Defaults are in-memory and the lean core.

```js
import { create } from '@tegmentum/ducklink';

const db   = await create({ coreUrl: '/ducklink_core.wasm' });
const conn = await db.connect();                 // or connect('/path/to.db')

const r = await conn.query('SELECT 42 AS answer, $1 AS who', ['world']);
r.toArray();      // [{ answer: 42n, who: 'world' }]  — typed JS objects
```

### Connection surface

```ts
create(opts?: CreateOptions): Promise<DuckLink>
DuckLink.connect(path?: string): Promise<Connection>

Connection.query(sql: string, params?: unknown[]): Promise<Result>
Connection.queryStream(sql, params?)         // AsyncIterable of rows
Connection.queryArrow(sql, params?)          // Arrow IPC escape hatch
Connection.prepare(sql)                       // prepared statement
Connection.load(name: string): Promise<this>  // load an extension component
Connection.install(name: string)             // fetch without loading
Connection.providerOf(name): string | undefined
Connection.loadedExtensions(): string[]
Connection.registerFileBuffer / registerFileText / registerFileHandle
Connection.insertArrowTable / appender
```

### The typed `Result`

`Result` defaults to **typed JS object rows** — the ergonomic win over juggling
raw value arrays — with a lazy Arrow escape hatch:

```ts
interface Result {
  readonly columns: ColumnMeta[];
  readonly columnNames: string[];
  readonly numRows: number;
  toArray(): Row[];               // Row = Record<string, DuckJsValue>
  toRows(): DuckJsValue[][];      // column-position rows
  get(i: number): Row | undefined;
  [Symbol.iterator](): Iterator<Row>;
  toArrow(): Promise<unknown>;    // apache-arrow, loaded on demand
}
```

Marshalling is BigInt-aware: `int64` / `uint64` come back as JS `BigInt` (note the
`42n` above). Errors are typed: `DuckLinkError`, `QueryError`, `ExtensionError`,
`ConformanceError`, `ContractError`, `InstantiationError`, `NotFoundError`.

## The sqlink facade

`@tegmentum/sqlink` mirrors the same `create` / `connect` / `query` / `load`
surface against the SQLite wasm core. `SqlLink` adds `version()` /
`versionNumber()`; its `Connection` adds `exec` and explicit transactions
(`begin` / `commit` / `rollback` / `inAutocommit`). It has no Arrow surface, and
its value type is `SqlJsValue = null | number | bigint | string | Uint8Array`.

## Browser runtime status (read this before shipping)

The runtime is **proven in headless Chromium via JSPI**, and the honest status is
narrower than the eventual design:

:::warning JSPI is required today; the worker fallback is a follow-on
The live path uses **JSPI** (`WebAssembly.Suspending`), so `create()` currently
requires **Chrome 137+** (or another JSPI-enabled runtime) and **throws** an
`InstantiationError` otherwise. The non-JSPI **Web Worker + postMessage sync
fallback** — running the core in sync transpile mode behind an id-correlated RPC,
with **no** `SharedArrayBuffer` / `Atomics` / COOP-COEP so it would run in any
browser with Web Workers — is **designed and scaffolded** (the RPC module
`createRpcClient` / `serveRpc` / `withTransfer` ships in `@tegmentum/datalink-browser`)
but **not yet wired into the facades**. It is a documented follow-on, not a
shipped feature.
:::

In-browser **extension dispatch** is wired for the ducklink facade (the corpus is
verified headless); the sqlink facade currently enforces the catalog conformance
gate in `load()` and throws until an extension host is supplied. Extensions are
resolved through the catalog — see [extension distribution](distribution.md).

## Versioning note

All three packages are published at `0.4.0` on npm. The package APIs evolve ahead
of the in-repo facade sources; treat the npm `0.4.0` release as the reference for
the surface above.
