# `fieldbook-component` -- the wasm `ducklink:fieldbook` engine

> **This is the sole `fieldbook_*` SQL surface.** The native
> `duckdb-fieldbook` DuckDB extension that used to ship the same functions
> in-process is [deprecated as of `duckdb-fieldbook 0.2.0`](https://github.com/tegmentum/duckdb-fieldbook/blob/main/DEPRECATED.md).
> Every DuckDB host that runs on ducklink -- native + ducklink extension
> (Direction 2), the standalone ducklink host (Direction 1), and
> DuckDB-Wasm in the browser -- loads THIS component.

A `duckdb:extension` wasm component that registers the fieldbook SQL
functions:

- Read (macros): `fieldbook_list()`, `fieldbook_entries(name)`,
  `fieldbook_source(name, ordinal)`
- Mutate (scalars): `fieldbook_create(name)`,
  `fieldbook_add_entry(name, source)`, `fieldbook_drop(name)`
- Record (scalar): `fieldbook_record_run(name, ordinal, run_id, duration_ms, status, error, row_count)`

State lives in three ordinary tables under the `__fieldbook_` prefix
(`__fieldbook_books`, `__fieldbook_entries`, `__fieldbook_runs`) --
created by the component at `load()` time via the
`duckdb:extension/nested-exec` host import. All the actual logic +
capability declaration live in the shared, DB-agnostic
[`fieldbook-core`](https://github.com/tegmentum/datalink/tree/main/extensions/fieldbook-core)
crate in the datalink repo; this component is a thin wasm shim over that.

## Deliberately no `fieldbook_run(name)` scalar

A scalar callback runs INSIDE the outer query engine and can't re-enter
it to execute arbitrary SQL. Execution of a fieldbook entry belongs in a
top-level host. The CLI orchestrator in
[`duckdb-fieldbook`](https://github.com/tegmentum/duckdb-fieldbook)
implements that: it reads `fieldbook_source(name, ord)`, executes the
SQL as its own top-level statement, then calls
`fieldbook_record_run(...)` to persist the outcome. Any other host that
wants to drive fieldbook execution should follow the same pattern.

## Build

```sh
cargo component build --release
# produces <ducklink-workspace-root>/target/wasm32-wasip1/release/fieldbook.wasm
```

Run through the native `ducklink` DuckDB extension:

```sh
DUCKLINK_COMPONENTS=fieldbook=/abs/path/to/fieldbook.wasm \
  duckdb -unsigned -c "\
    LOAD '/abs/path/to/ducklink.duckdb_extension'; \
    SELECT fieldbook_create('demo'); \
    SELECT fieldbook_add_entry('demo', 'SELECT 42');"
```

Or drive it via the CLI orchestrator (recommended for interactive use):

```sh
cd <duckdb-fieldbook-checkout>
cargo run --release --bin fieldbook -- --db mydata.duckdb
```

## Smoke test

`smoke.sql` + `smoke.expected` cover the full round-trip
(`fieldbook_create` -> `fieldbook_add_entry` -> `fieldbook_source` ->
top-level execute -> `fieldbook_record_run` -> read from
`__fieldbook_runs`). See the ducklink component-extension test harness
for how they're driven.

## Related

- [`fieldbook-core`](https://github.com/tegmentum/datalink/tree/main/extensions/fieldbook-core)
  (datalink) -- the DB-agnostic logic + capability declaration.
- [`duckdb-fieldbook`](https://github.com/tegmentum/duckdb-fieldbook) --
  the CLI orchestrator that boots this component via the ducklink
  extension.
- `docs/nested-exec-direction-1-plan.md` (this repo) -- why the CLI takes
  Direction 2 rather than embedding a standalone wasm host.
