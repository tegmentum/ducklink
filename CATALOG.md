# ducklink component-extension catalog

> Auto-generated from `registry/index.json` by `tooling/gen-catalog.py`. Do not edit by hand.

**111 component extensions** · **254 SQL functions** · 4 expose aggregates · 2 require network.

Every extension is a Rust `wasm32-wasip2` component implementing the `duckdb:extension` WIT world. Load at runtime with `LOAD <name>` (artifacts in `artifacts/extensions/`), or browse them at `ducklink serve`. None overlap DuckDB built-ins; each is verified by `tooling/smoke.py`.

## Capabilities

- **Scalars** — the default; pure per-row functions.
- **Aggregates** — `aggstat`, `bloom`, `minhash`, `countmin` use the whole-batch `call_aggregate` path.
- **Network** — `dns`, `http` need an outbound-network grant (`DUCKLINK_NETWORK_GRANT`), off by default.

## Loading & embedding

- **Runtime load (every extension):** `LOAD <name>;` pulls `artifacts/extensions/<name>.wasm` — no core recompile, version-independent. This is the component model's whole point.
- **Static embed (opt-in):** `ducklink compose --embed <name>` bakes an extension into the core at build time. Wired today for `isin` (`embed-isin` core feature); `ducklink compose --list` shows what's embeddable. Most extensions stay runtime-loaded by design.
- **Network grant:** net extensions are denied by default; opt in with `DUCKLINK_NETWORK_GRANT=all` or a name allowlist.

## Text & NLP (45)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **bbcode** | `bbcode_to_html` | hand-rolled |  |
| **braille** | `to_braille` | hand-rolled |  |
| **cardtype** | `card_brand` | hand-rolled |  |
| **casing** | `to_snake_case`, `to_camel_case`, `to_pascal_case`, `to_kebab_case`, `to_title_case`, `to_constant_case` | heck |  |
| **checkdigit** | `verhoeff_validate`, `verhoeff_append`, `damm_validate`, `damm_append` | hand-rolled |  |
| **email** | `email_validate`, `email_domain`, `email_local` | email_address |  |
| **emoji** | `emoji_name`, `emoji_shortcode`, `emoji_char` | emojis |  |
| **escape** | `html_escape`, `html_unescape`, `url_encode`, `url_decode` | html-escape, percent-encoding |  |
| **gravatar** | `gravatar_hash`, `gravatar_url` | md5 |  |
| **html2text** | `html_to_text` | nanohtml2text |  |
| **idna** | `idna_to_ascii`, `idna_to_unicode` | idna |  |
| **initials** | `initials`, `initials_dotted` | hand-rolled |  |
| **ipaddr** | `ip_valid`, `ip_version`, `ip_is_private` | hand-rolled |  |
| **isbn** | `isbn_valid`, `isbn_normalize` | hand-rolled |  |
| **isbnconv** | `isbn10_to_13`, `isbn13_to_10` | hand-rolled |  |
| **leetspeak** | `to_leet` | hand-rolled |  |
| **lorem** | `lorem_words`, `lorem_title` | lipsum |  |
| **luhngen** | `luhn_check_digit`, `luhn_append` | hand-rolled |  |
| **mac** | `mac_valid`, `mac_normalize` | macaddr |  |
| **markdown** | `md_to_html`, `md_to_text` | pulldown-cmark |  |
| **morse** | `morse_encode`, `morse_decode` | hand-rolled |  |
| **nato** | `nato` | hand-rolled |  |
| **natsort** | `natsort_compare` | natord |  |
| **ngrams** | `char_ngrams`, `word_ngrams` | serde_json |  |
| **numwords** | `num_to_words`, `num_to_ordinal_words` | num2words |  |
| **ordinal** | `ordinal` | hand-rolled |  |
| **phonenumber** | `phone_valid`, `phone_format`, `phone_country_code` | phonenumber |  |
| **phonetic** | `soundex`, `metaphone` | rphonetic |  |
| **phonetic2** | `nysiis`, `refined_soundex`, `double_metaphone` | rphonetic |  |
| **piglatin** | `to_pig_latin` | hand-rolled |  |
| **pluralize** | `pluralize`, `singularize` | pluralizer |  |
| **rapidfuzz** | `fuzz_ratio`, `damerau_levenshtein`, `indel`, `osa` | rapidfuzz |  |
| **rot13** | `rot13`, `caesar` | hand-rolled |  |
| **simhash** | `simhash`, `simhash_distance` | hand-rolled |  |
| **slug** | `slugify` | slug |  |
| **stem** | `stem` | rust-stemmers |  |
| **stopwords** | `is_stopword`, `remove_stopwords` | stop-words |  |
| **textstat** | `word_count`, `sentence_count`, `syllable_count`, `flesch_reading_ease`, `reading_time_minutes` | hand-rolled |  |
| **tiktoken** | `tiktoken_count`, `tiktoken_encode`, `tiktoken_decode` | tiktoken-rs |  |
| **transliterate** | `deunicode` | deunicode |  |
| **unicodenorm** | `nfc`, `nfd`, `nfkc`, `nfkd` | unicode-normalization |  |
| **unicodewidth** | `grapheme_count`, `display_width` | unicode-segmentation, unicode-width |  |
| **url** | `url_scheme`, `url_host`, `url_port`, `url_path`, `url_query` | url |  |
| **whatlang** | `detect_language`, `detect_language_name`, `detect_script` | whatlang |  |
| **wordwrap** | `word_wrap` | textwrap |  |

## Data types & encoding (44)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **aggstat** | `harmonic_mean` | hand-rolled | aggregate |
| **ascii85** | `ascii85_encode`, `ascii85_decode` | ascii85 |  |
| **base58check** | `base58check_encode`, `base58check_decode` | bs58 |  |
| **baseconv** | `from_base` | hand-rolled |  |
| **bech32** | `bech32_encode`, `bech32_hrp`, `bech32_decode_hex`, `bech32_valid` | bech32 |  |
| **bloom** | `bloom_filter`, `bloom_contains` | hand-rolled | aggregate |
| **cbor** | `cbor_from_json`, `cbor_to_json` | ciborium, serde_json, hex |  |
| **color** | `color_luminance`, `color_contrast` | csscolorparser |  |
| **colorconv** | `hex_to_hsl`, `hex_to_hsv`, `hsl_to_hex` | hand-rolled |  |
| **countmin** | `count_min`, `cms_estimate` | hand-rolled | aggregate |
| **csscolor** | `css_to_hex`, `css_to_rgb`, `css_valid` | csscolorparser |  |
| **currency** | `currency_name`, `currency_symbol`, `currency_numeric`, `currency_exponent` | iso_currency |  |
| **dice** | `dice_roll`, `dice_min`, `dice_max` | rand |  |
| **dms** | `dms_to_decimal`, `decimal_to_dms` | hand-rolled |  |
| **elements** | `element_name`, `element_number`, `element_weight` | mendeleev |  |
| **faker** | `fake_name`, `fake_email`, `fake_username`, `fake_city`, `fake_company` | fake |  |
| **geohash** | `geohash_encode`, `geohash_decode_lat`, `geohash_decode_lon` | geohash |  |
| **graycode** | `gray_encode`, `gray_decode` | hand-rolled |  |
| **hashids** | `hashids_encode`, `hashids_decode` | harsh |  |
| **haversine** | `haversine_km`, `haversine_mi` | hand-rolled |  |
| **humansize** | `humansize`, `humansize_binary` | humansize |  |
| **humantime** | `humantime_parse`, `humantime_format` | humantime |  |
| **idextra** | `ksuid`, `cuid2` | svix-ksuid, cuid2 |  |
| **ids** | `ulid`, `nanoid`, `ulid_timestamp` | ulid, nanoid |  |
| **iso** | `iso_country_name`, `iso_country_alpha3`, `iso_country_numeric` | rust_iso3166 |  |
| **jsonschema** | `json_schema_valid` | jsonschema, serde_json |  |
| **mime** | `mime_type`, `mime_from_ext` | mime_guess |  |
| **minhash** | `minhash`, `minhash_similarity` | hand-rolled | aggregate |
| **money** | `format_money` | iso_currency |  |
| **msgpack** | `msgpack_from_json`, `msgpack_to_json` | rmp-serde, serde_json, hex |  |
| **petname** | `petname` | petname |  |
| **pluscode** | `pluscode_encode`, `pluscode_valid`, `pluscode_decode_lat`, `pluscode_decode_lon` | open-location-code, geo |  |
| **qrcode** | `qr_svg` | qrcode |  |
| **quotedprintable** | `qp_encode`, `qp_decode` | quoted_printable |  |
| **rle** | `rle_encode`, `rle_decode` | hand-rolled |  |
| **roman** | `to_roman`, `from_roman` | roman |  |
| **semver** | `semver_valid`, `semver_major`, `semver_minor`, `semver_patch`, `semver_compare` | semver |  |
| **timezone** | `tz_valid`, `tz_offset_seconds`, `tz_abbreviation` | chrono-tz, chrono |  |
| **toml** | `toml_to_json`, `json_to_toml` | toml, serde_json |  |
| **unitconv** | `unit_convert` | hand-rolled |  |
| **uuid5** | `uuid_v5`, `uuid_v3` | uuid |  |
| **uuidx** | `uuid_v7`, `uuid_version`, `uuid_timestamp` | uuid |  |
| **yaml** | `yaml_to_json`, `json_to_yaml` | serde_yaml, serde_json |  |
| **z85** | `z85_encode`, `z85_decode` | z85 |  |

## Cryptography & security (12)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **atbash** | `atbash` | hand-rolled |  |
| **checksums** | `crc16`, `adler32`, `fnv1a_32`, `fnv1a_64` | crc, adler |  |
| **crypto** | `sha1`, `sha512`, `sha3_256`, `blake3`, `crc32` | sha1, sha2, sha3, blake3, crc32fast |  |
| **hmac** | `hmac_sha256`, `hmac_sha512` | hmac, sha2 |  |
| **jwt** | `jwt_header`, `jwt_payload` | base64 |  |
| **passgen** | `gen_password`, `gen_password_alnum` | rand |  |
| **passphrase** | `passphrase` | chbs |  |
| **pwstrength** | `password_score`, `password_strength` | passwords |  |
| **rot47** | `rot47` | hand-rolled |  |
| **siphash** | `siphash` | siphasher |  |
| **totp** | `totp` | hmac, sha1, base32 |  |
| **vigenere** | `vigenere_encrypt`, `vigenere_decrypt` | hand-rolled |  |

## Validators (6)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **aba** | `aba_validate` | hand-rolled |  |
| **creditcard** | `cc_validate`, `cc_network` | hand-rolled |  |
| **ean** | `ean_validate`, `ean_check_digit` | hand-rolled |  |
| **iban** | `iban_validate`, `iban_country`, `iban_bban` | hand-rolled |  |
| **isin** | `isin_validate`, `isin_check_digit`, `isin_country`, `isin_nsin` | hand-rolled |  |
| **luhn** | `luhn_validate`, `luhn_check_digit` | hand-rolled |  |

## Network (2)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **dns** | `dns_lookup`, `dns_resolve_all` | hand-rolled | network |
| **http** | `http_get`, `http_status` | rustls, rustls-rustcrypto, webpki-roots | network |

## Encoding (1)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **baseN** | `base32_encode`, `base32_decode`, `base58_encode`, `base58_decode` | base32, bs58 |  |

## Math (1)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **hashfuncs** | `xxh32`, `xxh64`, `xxh3`, `murmur3` | twox-hash, murmur3 |  |

## Also in the registry (not component extensions)

**DuckDB built-ins:** `_comment`, `autocomplete`, `core_functions`, `icu`, `json`, `parquet`, `tpcds`, `tpch`

**Official C++ extensions** (static-linked via `EMBED_EXTENSIONS`): `_comment`, `avro`, `aws`, `azure`, `ducklake`, `excel`, `fts`, `httpfs`, `iceberg`, `inet`, `mysql_scanner`, `postgres_scanner`, `spatial`, `sqlite_scanner`, `ui`, `vss`

