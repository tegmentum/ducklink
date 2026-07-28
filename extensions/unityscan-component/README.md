# `unityscan-component` -- Unity Catalog REST catalog for DuckLink

> A wasm `duckdb:extension` component that ATTACHes a Databricks / open
> [`unitycatalog`](https://github.com/unitycatalog/unitycatalog) endpoint over
> HTTPS (wasi:sockets + rustls) and serves its schemas, tables and columns
> through the storage-dispatch WIT interface.

```sql
LOAD unityscan;
ATTACH 'https://<workspace>;token=<pat>;catalog=<cat>' AS uc (TYPE unity);
SHOW ALL TABLES;                -- names via UC /schemas + /tables
DESCRIBE uc."default.trips";    -- columns from the table's columns[]
```

## Scope: catalog enumeration, not data scan

The **catalog enumeration** is the deliverable:

- `storage-attach` parses the DSN, stashes UC endpoint + PAT + catalog.
- `storage-list-tables` walks `/api/2.1/unity-catalog/schemas` then, for each
  schema, `/tables?catalog_name=&schema_name=`, returning fully-qualified
  `schema.table` names. Fetched `UcTable` rows (columns + `storage_location`)
  are cached to avoid a re-fetch on the follow-up columns call.
- `storage-table-columns` returns the cached `UcColumn` list mapped to DuckDB
  logical types (see `map_uc_type`).

The **data scan is intentionally out of scope for this component**: each UC
table points to a `storage_location` (`s3://…`, `az://…`, `file://…`) holding
delta or parquet files. A full `SELECT *` is meant to hand that location to
`s3fs` / `azfs` + the `delta` / `parquet` readers (compose them at LOAD time).
`storage-scan-next` therefore always returns EOF here — no rows are read
through this component.

The JSON REST surface (URL builders + parsers) is exhaustively unit-tested
offline against captured-shape UC responses; see `src/uc.rs`.

## Read-only at @5 (no write path)

`unityscan` is **read-only** on the `duckdb:extension@5.0.0` `storage-dispatch`
contract:

| Surface                              | Status                                                                     |
|--------------------------------------|-----------------------------------------------------------------------------|
| `storage-attach` / `storage-detach`  | Implemented                                                                 |
| `storage-list-tables`                | Implemented (walks UC REST `/schemas` + `/tables`)                          |
| `storage-table-columns`              | Implemented (synthetic `rowid` at index 0 for contract consistency; see below) |
| `storage-scan-open` / `-scan-close`  | Implemented (validate table resolves, return an empty cursor id)            |
| `storage-scan-next`                  | Always returns EOF (data scan runs through the composed FS + reader stack)  |
| `serialize` (write-back)             | **Rejected** with `Duckerror::Unsupported("unityscan is read-only: …")`     |
| Metadata mutations (CREATE / DROP)   | Not implemented — Unity Catalog admin ops live behind separate REST verbs   |
| Data mutations (INSERT / UPDATE / …) | Not applicable — data files sit at each table's `storage_location`          |

The `storage-dispatch@5` WIT has no `storage-insert-rows` / `-update-rows` /
`-delete-rows`; the only write-adjacent verb is `serialize`, which the host
calls after any successful write dispatch to persist a foreign catalog blob
back to its DSN. Unity Catalog is a remote REST endpoint, not a serializable
blob — `serialize` returns `Unsupported`, which the host treats as "no
write-back for this backend" and silently proceeds.

### Synthetic `rowid` column

`storage-table-columns` prepends a `rowid` (`Int64`) column at index 0 so
`at5_locate_rowid_column` in the host succeeds uniformly across every storage
backend (ADR Amendment A5 / `docs/at5-rowid-mechanism.md`). No `rowid` values
are ever emitted, because `storage-scan-next` never yields any rows.

## DSN

```
https://<host>;token=<pat>;catalog=<cat_name>
endpoint=<url> token=<pat> catalog=<cat_name>
```

Keys (`;`- or whitespace-separated, any order):

| Key                          | Meaning                                                             |
|------------------------------|---------------------------------------------------------------------|
| `endpoint` / `url` / `host`  | UC base URL (scheme + authority). Trailing path is stripped.        |
| `token` / `bearer` / `pat`   | UC personal access token. Omit for an unauthenticated open server.  |
| `catalog` / `catalog_name`   | UC catalog to enumerate. Defaults to `main`.                        |

The same keys can also be passed as `ATTACH` options — options override DSN
values.

The `endpoint=` form is preferred over a bare `http(s)://…` DSN. DuckDB's
`ATTACH` binder intercepts an `http(s)://`-prefixed path and demands the
`httpfs` extension (absent on the lean core); `endpoint=…` routes straight
to `TYPE unityscan`.

## Runtime dependencies

- `duckdb:extension@5.0.0` — the standard `duckdb:extension/*` imports.
- `wasi:sockets/*` + `wasi:io/*` — via `std::net::TcpStream` for the HTTPS
  client (rustls, RustCrypto crypto provider, embedded `webpki-roots` CA set).
- Host network grant — the DuckLink CLI must be launched with the appropriate
  `--grant-network` scope; otherwise all `http::get` calls fail before any UC
  request is made.

## Build

```sh
cargo component build -p unityscan-component --target wasm32-wasip1 --release
# -> target/wasm32-wasip1/release/unityscan.wasm
```

## Live smoke

Not registered in `smoke.py --all` — it needs an external UC endpoint and the
network grant. Use `smoke.sql.requires-live-server` against either the open
`unitycatalog` OSS server (`docker run -p 8080:8080 unitycatalog/unitycatalog`
ships catalog `unity`, schema `default`, tables `numbers` / `marksheet` /
`user_countries`) or a real Databricks workspace.

## Related

- [`unitycatalog`](https://github.com/unitycatalog/unitycatalog) — the open UC
  reference server this component targets.
- Official `unity_catalog` DuckDB extension (`src/uc_api.cpp`) — the REST
  endpoint set this component mirrors.
