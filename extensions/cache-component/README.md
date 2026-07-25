# `cache-component` -- the wasm `ducklink:cache` engine

> The wasm-side counterpart to the native [`duckdb-cache`](https://github.com/tegmentum/duckdb-cache) DuckDB extension. Same SQL surface, same on-disk layout — a database migrated between the two arms sees the same cache.

A `duckdb:extension` wasm component that registers two `cache` scalar overloads:

- `cache(uri VARCHAR) -> VARCHAR`
- `cache(config_json VARCHAR, uri VARCHAR) -> VARCHAR`

Given a URI, the scalar returns a `file://` URI pointing at a locally cached copy of the same bytes. `file://` inputs are returned unchanged. Anything else supported by the resolver is fetched, hashed, atomically published into the content-addressed store, and recorded in a SQLite catalog.

## Backend coverage (v0)

| Scheme | Status |
|---|---|
| `file://` | pass-through (identical to native) |
| `http://` / `https://` | full ETag / If-None-Match / Last-Modified / If-Modified-Since / 304 dance |
| `s3://` / `gs://` / `az://` / `azure://` | stubbed with the same "not yet supported" error the native uses; the sibling agent working on the native side adds `object_store` coverage there and the wasm arm picks up its own `object_store`-in-wasm story separately |

## On-disk layout parity

Byte-for-byte identical to the native `duckdb-cache`:

```
<cache_root>/
  metadata.db                     -- sqlite catalog
  objects/<sha256[:2]>/<sha256[2:]>
  locks/<sha256-of-source-uri>.lock
  tmp/<random>
```

The metadata schema comes verbatim from `cache-core::CREATE_CACHE_ENTRIES` (mirrors `duckdb-cache/src/schema.rs`), so either arm can open a `metadata.db` written by the other.

`<cache_root>` resolution mirrors the native's, with one WASI wrinkle: the wasm arm has no reliable notion of a per-user platform cache directory, so `Global` scope falls back to the same path as `Local` (`$DUCKLINK_LOCAL_CACHE` if set, otherwise `<cwd>/.ducklink/cache`). Explicit `$DUCKLINK_GLOBAL_CACHE` still wins.

## Persistent metadata

The catalog reads / writes go through `sqlite:extension/spi` (imported from sqlite-wasm's WIT contract). The catalog is intentionally byte-portable: any host that can satisfy `sqlite:extension/spi` — sqlite-lib pre-composed into the artifact via `wac compose`, or the ducklink CLI hosting the SPI natively — makes the extension fully functional.

## Locking caveat (v0)

The native takes an `fs2::FileExt::lock_exclusive` on `<cache_root>/locks/<sha256>.lock` to serialise concurrent misses on the same URI. WASI 0.2's `wasi:filesystem/types` exposes no flock primitive, so v0 of this shim **skips** the per-URI lock. Because the store is content-addressed and the publish is rename-atomic, concurrent misses converge on identical bytes at identical paths — the catalog's `ON CONFLICT (cache_name, source_uri) DO UPDATE` keeps the row consistent regardless of write ordering. The lock is therefore a **throughput** optimisation (avoids duplicate downloads) rather than a correctness one; the wasm arm accepts the duplicate-download cost in v0 in exchange for shipping.

A follow-up option: expose a lock primitive via a new WIT interface (`ducklink:cache/host-lock` or reuse `wasi:filesystem` once WASI-preview3 lands a lock ability).

## Compared to the native

| Aspect | native `duckdb-cache` | wasm `cache-component` |
|---|---|---|
| SQL surface | `cache(uri)` + `cache(config_json, uri)` | identical |
| Metadata catalog | rusqlite | `sqlite:extension/spi` (from sqlite-wasm) |
| HTTP client | `reqwest` blocking + rustls | `std::net::TcpStream` + rustls (matches `httpclient-component`) |
| Content-addressed store | `std::fs` + `fs2` flock | `std::fs` (WASI preopens) — no flock |
| Object-store schemes | stub in v0 | stub in v0 |
| TTL fast path in `Auto` | inspects `validated_at + ttl`, `expires_at` | v0 always revalidates on `Auto`; TTL fast path is a follow-up |

## Build

```sh
cargo component build -p cache-component --target wasm32-wasip1 --release
# produces <ducklink-workspace-root>/target/wasm32-wasip1/release/cache.wasm
```

## Runtime dependencies

The component imports:

- `duckdb:extension/*` — standard baseline (satisfied by the ducklink native extension and the standalone host).
- `duckdb:extension/nested-exec` — same host import fieldbook uses; v0 does not call it, but reserving the import here keeps future resolver evolutions from breaking the WIT world.
- `sqlite:extension/spi@0.1.0` — **new** import. Currently NOT satisfied by the ducklink CLI. To wire end-to-end:
  1. Pre-compose sqlite-lib into the artifact at build time: `wac compose cache.wasm --dep sqlite:extension/spi=sqlite_lib.component.wasm -o cache.composed.wasm`, then load the composed artifact instead of `cache.wasm`.
  2. Alternatively, host the SPI natively in `crates/ducklink-host` by opening a sibling sqlite3 connection against `<cache_root>/metadata.db` on first use.
- `wasi:filesystem/*` — for the content-addressed blob store. Ducklink CLI must preopen `<cache_root>` (or a parent).
- `wasi:sockets/*` (via `std::net::TcpStream`) — for the HTTP backend. Same requirement as `httpclient-component`.

Until the SPI import is wired, `LOAD cache;` will surface an unresolved-import error at instantiation. The `file://` pass-through does not exercise the SPI, but the extension still needs the import satisfied at load time.

## Related

- [`cache-core`](../../../datalink/extensions/cache-core) (datalink) — the DB-agnostic logic + capability declaration.
- [`duckdb-cache`](https://github.com/tegmentum/duckdb-cache) — the native reference this component tracks.
- [`sqlite-wasm`](https://github.com/tegmentum/sqlite-wasm) — the componentized SQLite `sqlite-lib` this component's SPI import points at.
