# DuckDB-wasm extension catalog

The comprehensive list of extensions we will implement as WebAssembly components
against the `duckdb:extension` world. This is the living roadmap the tooling
exists to drive:

- **Build one:** `make ext-scaffold NAME=<n> CRATE=<a,b>` → edit
  `extensions/<n>-component/src/lib.rs` → `make ext NAME=<n>-component`.
- **Track crates:** `tooling/compat-registry.json` (wasm32-wasip2 status per
  upstream crate) — `make ext-list-broken`.
- **Machine-readable catalog:** `registry/index.json` (what's built / planned).
- **Feedback:** `tooling/lessons-learned.md` + `python3 tooling/t-status.py`.

Mapped from `~/git/sqlite-wasm`'s ~103 shipped extensions, re-scored for DuckDB.

## Status legend

| tag | meaning |
| --- | --- |
| **IMPL** | DuckDB lacks it — implement as an extension. |
| **PARTIAL** | DuckDB has some of it; the extension extends/completes it. |
| **NATIVE** | DuckDB (or a first-party extension) already covers it — skip/deprioritize. |
| **DONE** | built + smoke-asserted in this repo. |

### What DuckDB already provides (→ NATIVE / PARTIAL)

CSV/Parquet/JSON/Arrow readers; most string/math/stats functions; `LIST`/`ARRAY`
+ `array_cosine_similarity` (and the VSS extension); `range`/`generate_series`;
SQL macros; `date_trunc`/`date_part`/`time_bucket`; `levenshtein`/`jaro_winkler`;
`md5`/`sha256`; `base64`/`hex`/`encode`/`decode`; `DECIMAL`; full-text-search
(`fts`) and spatial (`spatial`) extensions; `read_text`/`read_blob`. Anything in
that set is NATIVE or PARTIAL below.

## Batches

Mirrors sqlite-wasm's phased rollout. ~65–70 IMPL candidates total.

| batch | theme | rough count |
| --- | --- | --- |
| **B1** | validators + identifiers (pure scalars, low risk) | ~22 |
| **B2** | crypto + encoding | ~10 |
| **B3** | color / units / domain + text-NLP | ~12 |
| **B4** | sketches + geo | ~9 |
| **B5** | network / web + data formats | ~12 |
| **B6** | vector / ML + long-tail | ~8 |

## Pilots (DONE)

| extension | status | exports | crate(s) |
| --- | --- | --- | --- |
| isin | **DONE** | isin_validate, isin_check_digit, isin_country, isin_nsin | (hand-rolled) |
| baseN | **DONE** | base32_encode/decode, base58_encode/decode | base32, bs58 |

---

## B1 — Validators & identifiers (IMPL; DuckDB lacks all)

Pure scalars, no I/O, mostly hand-rolled or tiny crates. Highest value/risk ratio.

| extension | status | exports | crate(s) |
| --- | --- | --- | --- |
| isin | **DONE** | validate / check_digit / country / nsin | hand-rolled |
| iban | IMPL | iban_validate / iban_country / iban_bban | hand-rolled (mod-97) |
| creditcard | IMPL | cc_validate / cc_network | hand-rolled (Luhn) |
| luhn | IMPL | luhn_validate / luhn_check_digit | hand-rolled |
| aba | IMPL | aba_validate | hand-rolled |
| bic | IMPL | bic_validate / bic_country | hand-rolled |
| cusip | IMPL | cusip_validate / cusip_check_digit | hand-rolled |
| isbn | IMPL | isbn_validate / isbn10_to_13 | isbn |
| ean | IMPL | ean_validate / ean_check_digit | hand-rolled |
| vin | IMPL | vin_validate / vin_year | hand-rolled |
| container | IMPL | container_validate (ISO 6346) | hand-rolled |
| ssn | IMPL | ssn_validate | hand-rolled |
| mac | IMPL | mac_validate / mac_normalize | hand-rolled (compat: `_mac-address-parse`) |
| postcode | IMPL | postcode_validate (per country) | hand-rolled |
| phone | IMPL | phone_validate / phone_format / phone_country | phonenumber |
| email | IMPL | email_validate / email_domain | email_address |
| url | IMPL | url_parse / url_host / url_scheme | url |
| idna | IMPL | idna_to_ascii / idna_to_unicode | idna |
| uuid-extras | IMPL | uuid_v7 / uuid_version / uuid_timestamp | uuid |
| ids | IMPL | ulid / nanoid / snowflake_decode | ulid |
| iso | IMPL | iso3166 / iso4217 / iso639 lookups | rust_iso3166, iso_currency, isolang |
| punycode | IMPL | punycode_encode / punycode_decode | idna |

## B2 — Crypto & encoding (IMPL)

| extension | status | exports | crate(s) |
| --- | --- | --- | --- |
| baseN | **DONE** | base32/base58 enc/dec | base32, bs58 |
| crypto | IMPL | sha1 / sha512 / sha3 / blake3 / crc32 | sha1, sha2, sha3, blake3, crc32fast |
| crypto-auth | IMPL | hmac / jwt_verify / totp / bcrypt / argon2 | hmac, jsonwebtoken, totp-rs |
| crypto-keys | IMPL | ed25519_verify / x25519 / merkle_root | ed25519-dalek, x25519-dalek |
| bencode | IMPL | bencode_decode / bencode_encode | bt_bencode |
| codecs | IMPL | cbor / msgpack / yaml convert | ciborium, rmp-serde, serde_yaml |
| compress | IMPL | gzip / zstd / lz4 (de)compress | flate2(?), zstd, lz4_flex |
| crc | IMPL | crc16 / crc32 / crc64 | crc |
| ieee754 | IMPL | float_bits / bits_float | hand-rolled |
| hexdump | IMPL | hexdump | hand-rolled (compat: `_hex-dump`) |

Note: base64/hex are **NATIVE**; json1 is **NATIVE**.

## B3 — Color / units / domain + text-NLP

| extension | status | exports | crate(s) | tag |
| --- | --- | --- | --- | --- |
| color | IMPL | wcag_luminance / wcag_contrast | hand-rolled | IMPL |
| csscolor | IMPL | css_parse / css_to_rgb / css_to_hex | csscolorparser | IMPL |
| unitconv | IMPL | convert(value, from, to) | hand-rolled | IMPL |
| currency | IMPL | currency_symbol / currency_minor_units | iso_currency | IMPL |
| humansize | IMPL | humansize / parse_size | hand-rolled | IMPL |
| case | IMPL | snake/camel/pascal/kebab/title | heck | IMPL |
| escape | IMPL | url_escape / html_escape / sql_escape | hand-rolled | IMPL |
| natsort | IMPL | natural_compare | hand-rolled | IMPL |
| numfmt | PARTIAL | format_number (grouping/locale) | hand-rolled | PARTIAL (printf exists) |
| text-nlp | IMPL | soundex / metaphone / stem / markdown_to_text | rust-stemmers, htmd, pulldown-cmark | IMPL |
| template | IMPL | render(template, json) | minijinja | IMPL |
| roman | IMPL | to_roman / from_roman | hand-rolled (compat: `_roman-numerals`) | IMPL |
| morse | IMPL | to_morse / from_morse | hand-rolled (compat: `_morse-code`) | IMPL |
| cron | IMPL | cron_next / cron_matches | cron, chrono | IMPL |

`fuzzy`/`regexp`/`levenshtein`/`jaro_winkler` → **NATIVE/PARTIAL**.

## B4 — Sketches & geo

DuckDB uses HLL/t-digest internally but doesn't expose them as user functions.

| extension | status | exports | crate(s) | tag |
| --- | --- | --- | --- | --- |
| bloom | IMPL | bloom_add / bloom_contains | hand-rolled | IMPL |
| hyperloglog | IMPL | hll_add / hll_count | hand-rolled | IMPL |
| count_min | IMPL | cms_add / cms_estimate | hand-rolled | IMPL |
| sketches | IMPL | tdigest_quantile / minhash | hand-rolled | IMPL |
| geo | IMPL | h3 / geohash / maidenhead | h3o, geohash | IMPL |
| latlon | IMPL | latlon_parse / latlon_format | hand-rolled | IMPL |
| pmtiles | IMPL | pmtiles_read (table fn) | oxigdal-pmtiles | IMPL |
| geo-distance | PARTIAL | haversine | hand-rolled | PARTIAL (spatial ST_Distance) |

`geopoly`/`rtree` → **NATIVE** (spatial extension).

## B5 — Network / web + data formats

| extension | status | exports | crate(s) | tag |
| --- | --- | --- | --- | --- |
| http | IMPL | http_get / http_post (WASI http, host-side) | (host) | IMPL |
| dns | IMPL | dns_resolve / dns_reverse | hickory-resolver (host) | IMPL |
| web-parsers | IMPL | css_select / html_text | scraper, serde_json_path | IMPL |
| graphql | IMPL | graphql_parse / graphql_validate | graphql-parser | IMPL |
| excel | IMPL | excel_read (table fn) | calamine (needs-bootstrap) | IMPL |
| avro | IMPL | avro_read (table fn) | apache-avro | IMPL |
| ical | IMPL | ical_parse | icalendar | IMPL |
| semver | IMPL | semver_compare / semver_satisfies | semver | IMPL |
| sqlparse | IMPL | sql_format / sql_tables | sqlparser | IMPL |
| faker | IMPL | fake_name / fake_email / ... | fake | IMPL |
| detect | IMPL | detect_lang / detect_mime / slugify | whatlang, infer, slug | IMPL |
| mailto | IMPL | mailto_parse | hand-rolled | IMPL |

`arrow`/`parquet`/`csv`/`jsonpath`/`fileio` → **NATIVE/PARTIAL**.

## B6 — Vector / ML + long-tail

| extension | status | exports | crate(s) | tag |
| --- | --- | --- | --- | --- |
| bpe | IMPL | bpe_tokenize / bpe_count | tiktoken-rs | IMPL |
| onnx | IMPL (deprioritized, ~23 MB) | onnx_infer | tract-onnx (needs-bootstrap) | IMPL |
| shapefile | IMPL | shp_read (table fn) | shapefile | IMPL |
| protobuf | IMPL | pb_decode | prost | IMPL |
| gtfs | IMPL | gtfs_read | hand-rolled | IMPL |
| geoip | IMPL | geoip_lookup | maxminddb | IMPL |
| secp256k1 | IMPL | secp_verify / secp_recover | secp256k1 | IMPL |
| datasketches | IMPL | theta / frequent_items | hand-rolled | IMPL |

`vec`/`vec0`/`array_cosine_similarity` → **NATIVE** (+ VSS extension).

---

## Out of scope / NATIVE (not porting)

`json1`, `arrow`, `parquet`, `csv`, `fts`/text search, `spatial` (geopoly/rtree/
ST_*), `vec`/VSS, `base64`/`hex`, `uuid` v4, `series`/`generate_series`,
`define`/`closure`/`listargs` (macro tricks DuckDB already supports), `regexp`,
`stats`/most math — all already in DuckDB core or a first-party extension.
