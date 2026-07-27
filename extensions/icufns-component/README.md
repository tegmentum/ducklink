# icufns-component

ICU-style language-sensitive text scalars via `icu_collator` (pure-Rust
ICU4X with bundled CLDR tables). Exposes:

- `icu_sort_key(text, locale) -> text` — hex-encoded UCA collation sort
  key. `ORDER BY icu_sort_key(name, 'de')` is the practical workaround
  for `COLLATE de`: the keys compare bytewise in locale-correct order.
- `icu_compare(a, b, locale) -> int` — `-1`/`0`/`1` locale-aware
  comparison.
- `icu_casefold(text) -> text` — full Unicode case folding
  (locale-independent).
- Per-locale single-arg sort-key scalars: `icu_sortkey_en(text)`,
  `icu_sortkey_sv(text)`, `icu_sortkey_de(text)`.

NULL and unparseable-locale inputs return NULL. Never panics.

## @5 status: `COLLATE icu_<loc>` bindings are silently no-op'd

At `duckdb:extension@5.0.0` `collation.register_collation` returns
`Duckerror::Unsupported`:

- `crates/ducklink-runtime/src/extension.rs` implements
  `extension_collation::Host::register_collation` as a hard `Unsupported`
  error — `duckdb_create_collation` is not part of the DuckDB stable C
  API and there is no host-side path to wire an already-registered
  transform scalar into a real `CreateCollationInfo`. This is the "no
  C-API equivalent" bucket in the @5 ADR (Decision 5(b)).
- The extension now SWALLOWS the error at `load()` time (see the
  `let _ = collation::register_collation(...)` in `register_scalars`) so
  the sort-key + compare + casefold scalars still land successfully.

Net effect at @5:

- `LOAD icufns;` succeeds; every scalar above works.
- `SELECT * FROM t ORDER BY name COLLATE icu_sv;` — the COLLATE clause
  does NOT resolve to the locale-bound sort-key scalar. Use the explicit
  workaround: `SELECT * FROM t ORDER BY icu_sortkey_sv(name);`.

If DuckDB later publishes a stable-C-API `duckdb_create_collation` hook
the load path can be re-enabled by dropping the swallow.

## Build

```
cargo component build -p icufns-component --release --target wasm32-wasip2
```

Artifact: `target/wasm32-wasip1/release/icufns.wasm`.
