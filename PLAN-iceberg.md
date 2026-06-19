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

## Phase 2 — Gzip table metadata  · effort S — DONE

Catalogs/writers may store `vN.gz.metadata.json` (gzip). VERIFIED on wasi:
`iceberg_scan(dir, metadata_compression_codec='gzip', version='1')` reads a
gzipped metadata file (25 rows). zlib (already linked via httpfs) handles the
gunzip — no extra deps.

## Phase 3 — AWS SigV4 catalog auth (Glue / S3Tables)  · effort M — DONE

`AWSInput::{Get,Head,Delete,Post}Request` now sign with a **self-contained SigV4**
(SHA-256 + HMAC, no AWS SDK, no openssl-header plumbing) in
`cmake/iceberg-wasi/aws_wasi.inc`, injected into aws.cpp's `#ifdef __wasi__`
branch by `scripts/build-libduckdb-wasm.sh`; requests go out via `HTTPUtil`
(curl). Credentials come from an `s3`/`aws` DuckDB secret (`CREATE SECRET (TYPE
S3, KEY_ID, SECRET, REGION)`).

VERIFIED: `ATTACH '<wh>' AS x (TYPE ICEBERG, ENDPOINT '<host:port, no scheme>',
AUTHORIZATION_TYPE 'sigv4', SECRET s3sec)` + `SELECT` (20 rows) against a mock
that **independently recomputes the signature** (Python hashlib/hmac) — our
signature matched byte-for-byte on every request. Note: the sigv4 endpoint must
be scheme-less (upstream `DecomposeHost` splits on `/`). Real AWS uses valid
certs (the embedded CA bundle trusts them); the test used a self-signed cert with
`enable_curl_server_cert_verification=false`.

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

## Phase 6 — Snapshot selection / time travel  · effort S–M — DONE

VERIFIED on wasi: a 2-snapshot pyiceberg table reads 40 rows at HEAD and 10 rows
via `iceberg_scan(…, snapshot_from_id=<id>)` (the first snapshot). `version=…`
also works. No code changes needed.

## Cross-cutting — iceberg test harness  · effort S — DONE

`tooling/iceberg_smoke.py` (`make iceberg-smoke`) generates consistent pyiceberg
fixtures (deflate + snappy, partitioned, multi-snapshot, gzip-metadata) under the
gitignored `build/iceberg-fixtures/` and asserts the whole surface through
`duckdb-host` — 11 checks: read_avro (deflate/snappy), iceberg_scan
(local/snappy/partitioned), gzip metadata, time travel, remote HTTP (range
server), and REST catalog none/bearer/**sigv4** (the sigv4 mock recomputes the
signature). Servers run in background threads. Requires `pyiceberg[snappy]` +
`pyarrow`. **11/11 passing.**

Note the **stale-metadata gotcha**: the duckdb-iceberg repo's `data/persistent/*`
tables record wrong `file_size_in_bytes`, so they only read locally — the harness
uses pyiceberg-written tables instead.

## Sequencing

**B1** Phase 1 (snappy) + Phase 7 harness → broadest real-table coverage, cheap.
**B2** Phase 2 (gzip metadata) + Phase 6 (time travel) → verification-heavy.
**B3** Phase 3 (SigV4) + Phase 4 (vended creds) → unlocks AWS + credentialed REST.
**B4** Phase 5 (writes) → the big one; gated on a writable catalog.
