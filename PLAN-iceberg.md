# Iceberg on DuckLink — remaining surface

A roadmap for completing Apache Iceberg support on the wasm32-wasip2 DuckDB
component. The foundation is in place (see `registry/index.json` → `iceberg`,
`avro`); this plan covers the gaps, ordered by value-to-effort.

## Current state (working + verified)

- **Reads** — `iceberg_scan('table', allow_moved_paths=true)`, **local and
  remote** (HTTP/S3 via httpfs range requests). Partition columns, positional +
  equality deletes / deletion vectors (roaring linked).
- **Metadata fns** — `iceberg_metadata`, `iceberg_snapshots`, `iceberg_to_ducklake`.
- **REST catalog** — `ATTACH … (TYPE ICEBERG, ENDPOINT, AUTHORIZATION_TYPE 'none')`
  and bearer/oauth2 token via `CREATE SECRET (TYPE ICEBERG, TOKEN …)`. Handshake
  (`config`/`namespaces`/`tables`/`LoadTable`) runs over the curl HTTPUtil. Covers
  R2 Data Catalog / Lakekeeper / Polaris / Tabular.
- **Avro codecs** — deflate + null (`scripts/build-wasi-deps.sh`, deflate-only avro-c).

## Known limits (the surface to close)

| Gap | Impact | Effort |
|---|---|---|
| Snappy/lzma avro codecs | manifests written with `avro.codec=snappy/zstd` won't read | S |
| Gzip metadata.json (`*.gz.metadata.json`) | some catalogs compress table metadata | S |
| AWS SigV4 catalog auth (Glue, S3Tables) | AWS-native catalogs `ATTACH` is stubbed | M |
| Vended credentials → httpfs S3 | credentialed S3 storage from a REST catalog | M |
| Writes (CREATE/INSERT/DELETE/UPDATE) | read-only today | L |
| Time travel / snapshot selection robustness | `iceberg_scan(..., version=…/snapshot_from_timestamp=…)` | S–M |
| Test fixtures + smoke regression | no committed iceberg smoke test | S |

---

## Phase 1 — Codec coverage (read more real-world tables)  · effort S

Real Iceberg writers (Spark, Flink, Trino) often write **snappy**-compressed avro
manifests; some use **zstd**. Our avro-c was deflate-only.

- **Snappy** — DONE. Built snappy for wasi from the `~/git/snappy-wasm` source
  (`scripts/build-wasi-deps.sh` → `build/wasi-deps/snappy`), rebuilt the avro-c
  fork with `find_package(Snappy CONFIG)` re-enabled (`-DSNAPPY_CODEC`), merged
  `libsnappy.a`. Verified: `read_avro` + `iceberg_scan` on a snappy-manifest
  pyiceberg table (30 rows); deflate still works (regression checked).
- **lzma/zstd** — TODO. `~/git/xz-wasm` / `~/git/zstd-wasm` give the libs;
  flip `LZMA_FOUND FALSE` in the avro-c patch + `find_package(LibLZMA)` the same
  way snappy was done. (zstd avro codec is newer; confirm the fork's codec.c
  supports it.) Lower priority — snappy + deflate cover the vast majority.

## Phase 2 — Gzip table metadata  · effort S

Catalogs/writers may store `vN.gz.metadata.json` (gzip). DuckDB's iceberg supports
`metadata_compression_codec='gzip'`. Confirm the gzip path works on wasi (zlib is
already linked via httpfs); add a fixture. Mostly a verification + docs task.

## Phase 3 — AWS SigV4 catalog auth (Glue / S3Tables)  · effort M

Today `AWSInput::{Get,Head,Delete,Post}Request` + `sigv4.cpp` are stubbed (no AWS
SDK). To support AWS-native Iceberg catalogs:

- Implement SigV4 request signing **without** the AWS C++ SDK — reuse
  **`~/git/aws-sigv4-wasm`** (a dedicated SigV4 component) or port the signing
  into `aws.cpp`'s `#if defined(__wasi__)` branch, issuing the actual HTTP via
  `HTTPUtil` (curl, already working) instead of `Aws::Http`.
- Credentials from the existing DuckDB S3 secret chain (`s3_access_key_id`, …).
- **Verify** — against an S3 Tables / Glue endpoint (or a SigV4-checking mock).

## Phase 4 — Vended credentials → httpfs S3  · effort M

REST catalogs return temporary S3 credentials in the `LoadTableResult.config`
(`s3.access-key-id`, `s3.session-token`, …) — the `X-Iceberg-Access-Delegation:
vended-credentials` header is already sent. Wire those config values into the
httpfs S3 secret for the table's data reads so credentialed buckets work through
an attached catalog. **Verify** — mock catalog returning config creds + a private
(local-emulated) S3.

## Phase 5 — Writes  · effort L

`CREATE TABLE … AS`, `INSERT`, `DELETE`, `UPDATE`, snapshot commits. iceberg writes
go through a catalog (`POST/PUT` to the REST catalog + new parquet/manifest/
metadata files written to storage). Needs:

- The REST catalog write endpoints (`POST …/tables`, `POST …/tables/{t}` commit)
  over `HTTPUtil` — extend the mock catalog to accept commits.
- Parquet + avro-manifest **writing** on wasi (parquet write already works; avro
  write needs the avro-c writer path — verify it's compiled/working).
- **Verify** — `CREATE TABLE lake.ns.t AS SELECT …; INSERT …; SELECT` round-trip
  against the (now writable) mock catalog.

## Phase 6 — Snapshot selection / time travel  · effort S–M

Confirm `iceberg_scan(…, version='…')`, snapshot-id and `…_from_timestamp`
selection work on wasi; add fixtures (multi-snapshot pyiceberg table).

## Cross-cutting — iceberg test harness  · effort S

- A committed fixture generator (`scripts/gen-iceberg-fixtures.py`, pyiceberg) that
  produces consistent tables (deflate + snappy, partitioned, with deletes,
  multi-snapshot) under a gitignored dir.
- A smoke test (local + a spawned range-capable HTTP server + a mock REST catalog)
  wired into the existing `tooling/smoke.py` so iceberg regressions are caught.
- Note the **stale-metadata gotcha**: the duckdb-iceberg repo's `data/persistent/*`
  tables record wrong `file_size_in_bytes`, so they only read locally.

## Sequencing

**B1** Phase 1 (snappy) + Phase 7 harness → broadest real-table coverage, cheap.
**B2** Phase 2 (gzip metadata) + Phase 6 (time travel) → verification-heavy.
**B3** Phase 3 (SigV4) + Phase 4 (vended creds) → unlocks AWS + credentialed REST.
**B4** Phase 5 (writes) → the big one; gated on a writable catalog.
