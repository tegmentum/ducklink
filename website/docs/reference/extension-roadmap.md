---
id: extension-roadmap
title: Extension roadmap
sidebar_label: Extension roadmap
---

# Extension roadmap

The original batched roadmap of functionality to deliver as `duckdb:extension`
components, mapped from `~/git/sqlite-wasm`'s shipped extensions and re-scored for
DuckDB. Most of these have shipped — see [the catalog](../catalog.md) for the
current built set. This page is the planning map the tooling exists to drive.

The loop is: `make ext-scaffold NAME=<n> CRATE=<a,b>` → edit
`extensions/<n>-component/src/lib.rs` → `make ext NAME=<n>-component`. Crate status
per upstream crate lives in `tooling/compat-registry.json` (`make
ext-list-broken`); the machine-readable catalog is `registry/index.json`.

## Status legend

| tag | meaning |
|---|---|
| **IMPL** | DuckDB lacks it — implement as an extension. |
| **PARTIAL** | DuckDB has some of it; the extension extends/completes it. |
| **NATIVE** | DuckDB (or a first-party extension) already covers it — skip. |
| **DONE** | built + smoke-asserted in this repo. |

## What DuckDB already provides (NATIVE / PARTIAL)

CSV/Parquet/JSON/Arrow readers; most string/math/stats functions; `LIST`/`ARRAY` +
`array_cosine_similarity` (and the VSS extension); `range`/`generate_series`; SQL
macros; `date_trunc`/`date_part`/`time_bucket`; `levenshtein`/`jaro_winkler`;
`md5`/`sha256`; `base64`/`hex`/`encode`/`decode`; `DECIMAL`; full-text-search
(`fts`) and spatial (`spatial`); `read_text`/`read_blob`.

## Batches

Mirrors sqlite-wasm's phased rollout (~65–70 IMPL candidates total):

| batch | theme | rough count |
|---|---|---|
| **B1** | validators + identifiers (pure scalars, low risk) | ~22 |
| **B2** | crypto + encoding | ~10 |
| **B3** | color / units / domain + text-NLP | ~12 |
| **B4** | sketches + geo | ~9 |
| **B5** | network / web + data formats | ~12 |
| **B6** | vector / ML + long-tail | ~8 |

### B1 — Validators & identifiers

Pure scalars, no I/O, mostly hand-rolled or tiny crates — highest value/risk
ratio. Examples: `isin` (DONE), `iban`, `creditcard`, `luhn`, `aba`, `bic`,
`cusip`, `isbn`, `ean`, `vin`, `container`, `ssn`, `mac`, `postcode`, `phone`,
`email`, `url`, `idna`, `uuid-extras`, `ids` (ulid/nanoid), `iso`, `punycode`.

### B2 — Crypto & encoding

`baseN` (DONE), `crypto` (sha1/sha512/sha3/blake3/crc32), `crypto-auth` (hmac/jwt/
totp), `crypto-keys` (ed25519/x25519/merkle), `bencode`, `codecs` (cbor/msgpack/
yaml), `compress` (gzip/zstd/lz4), `crc`, `ieee754`, `hexdump`. (base64/hex/json1
are NATIVE.)

### B3 — Color / units / domain + text-NLP

`color`, `csscolor`, `unitconv`, `currency`, `humansize`, `case`, `escape`,
`natsort`, `numfmt` (PARTIAL), `text-nlp` (soundex/metaphone/stem/markdown),
`template` (minijinja), `roman`, `morse`, `cron`. (`fuzzy`/`regexp`/`levenshtein`/
`jaro_winkler` are NATIVE/PARTIAL.)

### B4 — Sketches & geo

DuckDB uses HLL/t-digest internally but doesn't expose them as user functions:
`bloom`, `hyperloglog`, `count_min`, `sketches` (tdigest/minhash), `geo` (h3/
geohash/maidenhead), `latlon`, `pmtiles`, `geo-distance` (haversine, PARTIAL).
(`geopoly`/`rtree` are NATIVE via spatial.)

### B5 — Network / web + data formats

`http`, `dns`, `web-parsers` (css_select/html_text), `graphql`, `excel`, `avro`,
`ical`, `semver`, `sqlparse`, `faker`, `detect` (lang/mime/slug), `mailto`.
(`arrow`/`parquet`/`csv`/`jsonpath`/`fileio` are NATIVE/PARTIAL.)

### B6 — Vector / ML + long-tail

`bpe` (tiktoken), `onnx` (deprioritized, ~23 MB), `shapefile`, `protobuf`, `gtfs`,
`geoip`, `secp256k1`, `datasketches`. (`vec`/`vec0`/`array_cosine_similarity` are
NATIVE + VSS.)

## Out of scope / NATIVE (not porting)

`json1`, `arrow`, `parquet`, `csv`, `fts`/text search, `spatial`
(geopoly/rtree/`ST_*`), `vec`/VSS, `base64`/`hex`, `uuid` v4,
`series`/`generate_series`, macro tricks DuckDB already supports, `regexp`,
`stats`/most math — all already in DuckDB core or a first-party extension.
