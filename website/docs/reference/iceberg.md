---
id: iceberg
title: Iceberg on wasm
sidebar_label: Iceberg
---

# Iceberg on ducklink

Apache Iceberg support on the `wasm32-wasip2` DuckDB component. The foundation is
in place (the [`iceberg` and `avro` official extensions](official-extensions.md));
this page records what works, what's deferred, and the upstream gaps.

## Current state (working + verified)

- **Reads** — `iceberg_scan('table', allow_moved_paths=true)`, **local and
  remote** (HTTP/S3 via httpfs range requests). Partition columns, positional +
  equality deletes / deletion vectors (roaring linked).
- **Metadata functions** — `iceberg_metadata`, `iceberg_snapshots`,
  `iceberg_to_ducklake`.
- **REST catalog** — `ATTACH … (TYPE ICEBERG, ENDPOINT, AUTHORIZATION_TYPE 'none')`
  and bearer/oauth2 token via `CREATE SECRET (TYPE ICEBERG, TOKEN …)`. Covers R2
  Data Catalog / Lakekeeper / Polaris / Tabular.
- **AWS SigV4 catalog auth** (Glue, S3Tables) — a self-contained SigV4 signer
  (SHA-256 + HMAC, no AWS SDK) in `cmake/iceberg-wasi/aws_wasi.inc`; requests go
  out via the curl `HTTPUtil`. Credentials come from an `s3`/`aws` DuckDB secret.
- **Vended credentials → httpfs S3** — a REST catalog that vends
  `s3.access-key-id`/`secret-access-key`/`region`/`endpoint` reads `s3://` data
  with no global S3 settings.
- **Writes** — `CREATE TABLE` + `INSERT` against a REST catalog. The avro-c writer
  writes manifests; DuckDB's parquet writer writes data; the extension commits via
  `POST …/tables/{table}`. Verified persisting across processes.
- **Avro codecs** — deflate + snappy + xz (the avro-c lzma codec was patched to
  the standard `xz` container, since avro-c shipped it non-interoperable as raw
  LZMA2 named `"lzma"`).
- **Time travel** — `iceberg_scan(…, snapshot_from_id=…)` / `version=…`.

`make iceberg-smoke` runs the regression (`tooling/iceberg_smoke.py`): 11 checks
spanning read_avro (deflate/snappy/xz), iceberg_scan (local/snappy/partitioned),
gzip metadata, time travel, remote HTTP, and REST catalog none/bearer/sigv4.
**11/11 passing.**

:::warning Stale-metadata gotcha
The duckdb-iceberg repo's `data/persistent/*` tables record wrong
`file_size_in_bytes`, so they only read locally — the harness uses pyiceberg-written
tables instead.
:::

## Protocol notes for a writable catalog

- Use `ATTACH … SUPPORT_STAGE_CREATE true`.
- `VerifyTableExistence` is a HEAD (must 404 for missing tables, else the lookup
  pre-populates the entry → "already exists").
- The not-found load error type must be `NoSuchIcebergTableException`.
- The create POST omits `location` (the catalog assigns it).
- Stage on create (`_create_staged_table`), then `commit_table` the staged table.

## Upstream gaps — documented, not implemented

These throw `NotImplementedException`/`BinderException` in the **extension
itself** (verified live where noted), so they are not a wasm problem and not
ducklink's work — recorded so users aren't surprised:

- **DELETE** — `IRCatalog::PlanDelete` throws (verified).
- **UPDATE** — `BindUpdateConstraints` throws (verified).
- **INSERT into a partitioned table** — `iceberg_insert.cpp` throws.
- **Targeted-column INSERT, `RETURNING`, `ON CONFLICT`, Iceberg V3 tables** —
  explicit upstream throws.
- **DROP TABLE … CASCADE**, CREATE INDEX/VIEW, ALTER — unimplemented upstream.
- **zstd avro codec** — not in the avro-c fork (deflate/snappy/xz work). Supporting
  it means adding a codec to `codec.c` + linking `libzstd` (rare for manifests).

`CREATE TABLE AS` is CREATE + INSERT fused, both of which work, so it is expected
to work (verification is pure test coverage).
