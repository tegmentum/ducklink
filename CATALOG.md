# ducklink component-extension catalog

> Auto-generated from `registry/index.json` by `tooling/gen-catalog.py`. Do not edit by hand.

**164 component extensions** · **413 SQL functions** · 6 expose aggregates · 3 require network.

Every extension is a Rust `wasm32-wasip2` component implementing the `duckdb:extension` WIT world. Load at runtime with `LOAD <name>` (artifacts in `artifacts/extensions/`), or browse them at `ducklink serve`. None overlap DuckDB built-ins; each is verified by `tooling/smoke.py`.

## Capabilities

- **Scalars** — the default; pure per-row functions.
- **Aggregates** — `aggstat`, `bloom`, `minhash`, `countmin`, `bitfilters`, `tdigest` use the whole-batch `call_aggregate` path.
- **Network** — `dns`, `httpclient`, `openprompt` need an outbound-network grant (`DUCKLINK_NETWORK_GRANT`), off by default.

## Loading & embedding

- **Runtime load (every extension):** `LOAD <name>;` pulls `artifacts/extensions/<name>.wasm` — no core recompile, version-independent. This is the component model's whole point.
- **Static embed (opt-in):** `ducklink compose --embed <name>` bakes an extension into the core at build time. Wired today for `isin` (`embed-isin` core feature); `ducklink compose --list` shows what's embeddable. Most extensions stay runtime-loaded by design.
- **Network grant:** net extensions are denied by default; opt in with `--grant-network all` (or a name allowlist), equivalently the `DUCKLINK_NETWORK_GRANT` env var.

## Data types & encoding (68)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **aggstat** | `harmonic_mean` | hand-rolled | aggregate |
| **ascii85** | `ascii85_encode`, `ascii85_decode` | ascii85 |  |
| **base58check** | `base58check_encode`, `base58check_decode` | bs58 |  |
| **baseconv** | `from_base` | hand-rolled |  |
| **bech32** | `bech32_encode`, `bech32_hrp`, `bech32_decode_hex`, `bech32_valid` | bech32 |  |
| **bibtex** | `bibtex_count`, `bibtex_to_json`, `bibtex_keys` | biblatex |  |
| **bloom** | `bloom_filter`, `bloom_contains` | hand-rolled | aggregate |
| **cbor** | `cbor_from_json`, `cbor_to_json` | ciborium, serde_json, hex |  |
| **color** | `color_luminance`, `color_contrast` | csscolorparser |  |
| **colorconv** | `hex_to_hsl`, `hex_to_hsv`, `hsl_to_hex` | hand-rolled |  |
| **countmin** | `count_min`, `cms_estimate` | hand-rolled | aggregate |
| **csscolor** | `css_to_hex`, `css_to_rgb`, `css_valid` | csscolorparser |  |
| **currency** | `currency_name`, `currency_symbol`, `currency_numeric`, `currency_exponent` | iso_currency |  |
| **dbf** | `read_dbf` | dbase |  |
| **dice** | `dice_roll`, `dice_min`, `dice_max` | rand |  |
| **dms** | `dms_to_decimal`, `decimal_to_dms` | hand-rolled |  |
| **dotenv** | `dotenv_to_json`, `dotenv_get`, `dotenv_keys` | hand-rolled |  |
| **elements** | `element_name`, `element_number`, `element_weight` | mendeleev |  |
| **faker** | `fake_name`, `fake_email`, `fake_username`, `fake_city`, `fake_company` | fake |  |
| **fit** | `read_fit` | fitparser |  |
| **geohash** | `geohash_encode`, `geohash_decode_lat`, `geohash_decode_lon` | geohash |  |
| **graycode** | `gray_encode`, `gray_decode` | hand-rolled |  |
| **h3** | `h3_latlng_to_cell`, `h3_cell_to_lat`, `h3_cell_to_lng`, `h3_cell_to_parent`, `h3_grid_distance`, `h3_is_valid_cell` | h3o |  |
| **hashids** | `hashids_encode`, `hashids_decode` | harsh |  |
| **haversine** | `haversine_km`, `haversine_mi` | hand-rolled |  |
| **hocon** | `hocon_to_json`, `hocon_get` | hocon |  |
| **humansize** | `humansize`, `humansize_binary` | humansize |  |
| **humantime** | `humantime_parse`, `humantime_format` | humantime |  |
| **ical** | `ical_event_count`, `ical_to_json`, `ical_summaries` | ical, serde_json |  |
| **idextra** | `ksuid`, `cuid2` | svix-ksuid, cuid2 |  |
| **ids** | `ulid`, `nanoid`, `ulid_timestamp` | ulid, nanoid |  |
| **ini** | `ini_to_json`, `ini_get`, `ini_sections` | rust-ini, serde_json |  |
| **ion** | `ion_to_json`, `ion_from_json`, `ion_get` | ion-rs |  |
| **iso** | `iso_country_name`, `iso_country_alpha3`, `iso_country_numeric` | rust_iso3166 |  |
| **jaq** | `jq`, `jq_first` | jaq-core, jaq-std, jaq-json |  |
| **json_schema** | `json_schema_valid`, `json_schema_errors` | jsonschema |  |
| **jsonfns** | `json_valid`, `json_extract`, `json_extract_string`, `json_array_length`, `json_type`, `json_keys`, `json_contains`, `json_quote`, `to_json` | serde_json, serde_json_path |  |
| **jsonschema** | `json_schema_valid` | jsonschema, serde_json |  |
| **lindel** | `morton_encode`, `morton_decode_x`, `morton_decode_y`, `hilbert_encode`, `hilbert_decode_x`, `hilbert_decode_y` | hand-rolled |  |
| **magic** | `magic_mime`, `magic_extension`, `magic_matcher_type`, `is_image` | infer |  |
| **maidenhead** | `to_maidenhead`, `maidenhead_lat`, `maidenhead_lon` | hand-rolled |  |
| **marisa** | `fst_contains`, `fst_prefix`, `fst_count` | fst |  |
| **mime** | `mime_type`, `mime_from_ext` | mime_guess |  |
| **minhash** | `minhash`, `minhash_similarity` | hand-rolled | aggregate |
| **money** | `format_money` | iso_currency |  |
| **msgpack** | `msgpack_from_json`, `msgpack_to_json` | rmp-serde, serde_json, hex |  |
| **petname** | `petname` | petname |  |
| **plist** | `plist_to_json`, `plist_get` | plist |  |
| **pluscode** | `pluscode_encode`, `pluscode_valid`, `pluscode_decode_lat`, `pluscode_decode_lon` | open-location-code, geo |  |
| **polyline** | `polyline_encode`, `polyline_decode` | polyline |  |
| **qrcode** | `qr_svg` | qrcode |  |
| **quotedprintable** | `qp_encode`, `qp_decode` | quoted_printable |  |
| **rle** | `rle_encode`, `rle_decode` | hand-rolled |  |
| **roman** | `to_roman`, `from_roman` | roman |  |
| **semver** | `semver_valid`, `semver_major`, `semver_minor`, `semver_patch`, `semver_compare` | semver |  |
| **shapefile** | `read_shp` | shapefile |  |
| **timezone** | `tz_valid`, `tz_offset_seconds`, `tz_abbreviation` | chrono-tz, chrono |  |
| **toml** | `toml_to_json`, `json_to_toml` | toml, serde_json |  |
| **tsid** | `tsid_encode`, `tsid_decode`, `tsid_timestamp`, `tsid_from_timestamp` | hand-rolled |  |
| **unitconv** | `unit_convert` | hand-rolled |  |
| **uuid5** | `uuid_v5`, `uuid_v3` | uuid |  |
| **uuid7** | `uuid7_build`, `uuid7_timestamp`, `uuid7_is_valid` | uuid |  |
| **uuidx** | `uuid_v7`, `uuid_version`, `uuid_timestamp` | uuid |  |
| **vcard** | `vcard_count`, `vcard_to_json`, `vcard_names` | ical, serde_json |  |
| **warc** | `read_warc` | warc |  |
| **xml** | `xml_valid`, `xml_extract`, `xml_extract_all` | roxmltree |  |
| **yaml** | `yaml_to_json`, `json_to_yaml` | serde_yaml, serde_json |  |
| **z85** | `z85_encode`, `z85_decode` | z85 |  |

## Text & NLP (52)

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
| **html** | `html_extract`, `html_extract_all`, `html_attr` | scraper |  |
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
| **numfmt** | `num_group`, `num_si` | hand-rolled |  |
| **numwords** | `num_to_words`, `num_to_ordinal_words` | num2words |  |
| **ordinal** | `ordinal` | hand-rolled |  |
| **phonenumber** | `phone_valid`, `phone_format`, `phone_country_code` | phonenumber |  |
| **phonetic** | `soundex`, `metaphone` | rphonetic |  |
| **phonetic2** | `nysiis`, `refined_soundex`, `double_metaphone` | rphonetic |  |
| **piglatin** | `to_pig_latin` | hand-rolled |  |
| **pinyin** | `to_pinyin`, `to_pinyin_plain`, `to_pinyin_initials` | pinyin |  |
| **pluralize** | `pluralize`, `singularize` | pluralizer |  |
| **rapidfuzz** | `fuzz_ratio`, `damerau_levenshtein`, `indel`, `osa` | rapidfuzz |  |
| **rot13** | `rot13`, `caesar` | hand-rolled |  |
| **simhash** | `simhash`, `simhash_distance` | hand-rolled |  |
| **slug** | `slugify` | slug |  |
| **sqlformat** | `sql_format`, `sql_format_compact` | sqlformat |  |
| **stem** | `stem` | rust-stemmers |  |
| **stopwords** | `is_stopword`, `remove_stopwords` | stop-words |  |
| **textdiff** | `text_diff`, `diff_ratio`, `diff_changed_lines` | similar |  |
| **textlines** | `split_lines` | hand-rolled |  |
| **textplot** | `plot_sparkline`, `plot_bars`, `qr_utf8` | qrcode |  |
| **textstat** | `word_count`, `sentence_count`, `syllable_count`, `flesch_reading_ease`, `reading_time_minutes` | hand-rolled |  |
| **tiktoken** | `tiktoken_count`, `tiktoken_encode`, `tiktoken_decode` | tiktoken-rs |  |
| **transliterate** | `deunicode` | deunicode |  |
| **unicodenorm** | `nfc`, `nfd`, `nfkc`, `nfkd` | unicode-normalization |  |
| **unicodewidth** | `grapheme_count`, `display_width` | unicode-segmentation, unicode-width |  |
| **url** | `url_scheme`, `url_host`, `url_port`, `url_path`, `url_query` | url |  |
| **whatlang** | `detect_language`, `detect_language_name`, `detect_script` | whatlang |  |
| **wordwrap** | `word_wrap` | textwrap |  |

## Cryptography & security (13)

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
| **secp256k1** | `secp256k1_pubkey`, `secp256k1_sign`, `secp256k1_verify` | k256 |  |
| **siphash** | `siphash` | siphasher |  |
| **totp** | `totp` | hmac, sha1, base32 |  |
| **vigenere** | `vigenere_encrypt`, `vigenere_decrypt` | hand-rolled |  |

## Math (6)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **bitfilters** | `xor_filter`, `xor_filter_contains` | xorf, bincode | aggregate |
| **celestial** | `equatorial_to_galactic_l`, `equatorial_to_galactic_b`, `angular_separation`, `hms_to_deg`, `dms_to_deg` | hand-rolled |  |
| **frequentitems** | `top_k`, `top_k_value` | hand-rolled |  |
| **hashfuncs** | `xxh32`, `xxh64`, `xxh3`, `murmur3` | twox-hash, murmur3 |  |
| **stochastic** | `normal_cdf`, `normal_pdf`, `normal_quantile`, `binomial_pmf`, `poisson_pmf`, `exponential_cdf`, `beta_cdf` | statrs |  |
| **tdigest** | `tdigest`, `tdigest_quantile`, `tdigest_count` | tdigest, bincode | aggregate |

## Networking (6)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **inetfns** | `host`, `family`, `netmask`, `network`, `broadcast`, `inet_contains` | ipnetwork |  |
| **mailto** | `mailto_to`, `mailto_field`, `mailto_to_json` | percent-encoding |  |
| **netquack** | `registrable_domain`, `public_suffix`, `subdomain`, `domain_label` | psl |  |
| **openprompt** | `prompt`, `prompt_model` | rustls, serde_json | network |
| **urlpattern** | `url_pattern_test`, `url_pattern_match` | urlpattern |  |
| **useragent** | `ua_browser`, `ua_browser_version`, `ua_os`, `ua_category`, `ua_is_bot` | woothee |  |

## Validators (6)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **aba** | `aba_validate` | hand-rolled |  |
| **creditcard** | `cc_validate`, `cc_network` | hand-rolled |  |
| **ean** | `ean_validate`, `ean_check_digit` | hand-rolled |  |
| **iban** | `iban_validate`, `iban_country`, `iban_bban` | hand-rolled |  |
| **isin** | `isin_validate`, `isin_check_digit`, `isin_country`, `isin_nsin` | hand-rolled |  |
| **luhn** | `luhn_validate`, `luhn_check_digit` | hand-rolled |  |

## Encoding (4)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **base45** | `base45_encode`, `base45_decode` | base45 |  |
| **baseN** | `base32_encode`, `base32_decode`, `base58_encode`, `base58_decode` | base32, bs58 |  |
| **bencode** | `bencode_to_json`, `bencode_is_valid` | serde_bencode |  |
| **hexdump** | `hexdump`, `hex_pretty` | hand-rolled |  |

## Utility (4)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **cron** | `cron_is_valid`, `cron_next`, `cron_prev` | croner |  |
| **parsertools** | `sql_tables`, `sql_is_valid`, `sql_statement_type` | sqlparser |  |
| **prql** | `prql_to_sql`, `prql_is_valid` | prqlc |  |
| **rhai** | `rhai_eval`, `rhai_eval_int`, `rhai_eval_double` | rhai |  |

## Import Export (3)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **avrofns** | `avro_schema`, `read_avro`, `avro_record_count` | apache-avro |  |
| **excelfns** | `xlsx_sheets`, `read_xlsx`, `xlsx_cell` | calamine |  |
| **sqlitewasm** | `sqlite_blob_scan` | rusqlite |  |

## Network (2)

| Extension | Functions | Backed by | Notes |
|---|---|---|---|
| **dns** | `dns_lookup`, `dns_resolve_all` | hand-rolled | network |
| **httpclient** | `http_get`, `http_status`, `http_post` | rustls, rustls-rustcrypto, webpki-roots | network |

## Also in the registry (not component extensions)

**DuckDB built-ins:** `_comment`, `autocomplete`, `core_functions`, `icu`, `json`, `parquet`, `tpcds`, `tpch`

**Official C++ extensions** (static-linked via `EMBED_EXTENSIONS`): `_comment`, `avro`, `aws`, `azure`, `ducklake`, `excel`, `fts`, `httpfs`, `iceberg`, `inet`, `mysql_scanner`, `postgres_scanner`, `spatial`, `sqlite_scanner`, `ui`, `vss`

