# Lessons learned — DuckDB-wasm extensions

A running retrospective. Every time we build a componentized extension, the
friction we hit becomes a *tooling* item: something the scaffolder, the smoke
harness, the compat-registry, or the core/CLI should do better. Implementations
drive the tooling design, not the other way round.

This mirrors the feedback loop in `~/git/sqlite-wasm/tooling/lessons-learned.md`.

## The T-N convention

Tooling items are tagged inline with markers that `tooling/t-status.py` scans:

- `(T-N new)`    — opens tooling item N. Put a short title right after it.
- `(T-N closed)` — closes item N (any sub-clause works: "closed inline",
  "closed in same doc", "silently closed", ...).

An item is **open** if it has a `new` marker and no `closed` marker. Numbers are
allocated once and never reused. Run:

```
python3 tooling/t-status.py            # all (open first, then closed)
python3 tooling/t-status.py open       # just the open ones
python3 tooling/t-status.py closed     # just the closed ones
```

## Retrospectives

### 2026-06-17 tooling bring-up (registry + scaffold + smoke + feedback)

Ported the sqlite-wasm extension system to DuckDB: `registry/index.json`,
`tooling/compat-registry.json` (seeded verbatim — the upstream crates are
DB-agnostic), `tooling/scaffold.py` + `tooling/templates/`, `tooling/smoke.py`,
`tooling/t-status.py`, and the `make ext*` targets.

The big adaptation vs sqlite-wasm: DuckDB extensions register **imperatively**
in `load()` (open the host scalar/table/aggregate capability registry, call
`registry.register(...)`, wire a `callback-dispatch` export) rather than
returning a static `describe()` manifest. So `tooling/templates/lib.rs.tmpl`
differs from sqlite's, but the surrounding tooling maps 1:1.

Two structural facts shaped the harness:

- The wac-composed **standalone CLI links a no-op loader stub** and cannot
  instantiate extension components (`request_load` returns false). So smoke
  cannot run through the standalone — it runs through the **native host runner**
  `ducklink` (crates/ducklink-host), which has the real wasmtime
  loader and resolves `artifacts/extensions/<name>.wasm`. `tooling/smoke.py`
  drives that binary. (T-3 new) smoke harness spawns one `ducklink` process
  per extension and runs them serially; for a large catalog this wants a
  `-j`/parallel mode like sqlite-wasm's smoke.py grew. Left open until the
  catalog is big enough to feel it.

- (T-4 new) the scaffolder's build-check runs `cargo check`, not
  `cargo component build`, so it confirms the crate compiles but not that the
  component actually wraps. Fast (good for the inner loop) but a clean scaffold
  can still fail `make ext`. Consider an opt-in `--component-check`. Open.

### 2026-06-17 isin (hand-rolled validator pilot)

First real extension. Four scalars (`isin_validate`/`_check_digit`/`_country`/
`_nsin`), Luhn-mod-10 over the letter-expanded body. DB-agnostic algorithm
lifted from sqlite-wasm's `isin`; only the registration ABI changed. Compiled
clean off the scaffold; the scalar registration + dispatch pattern from
`sample-extension-component` transferred directly.

Hit the first real tooling bug while wiring the smoke harness:

(T-1 new) CLI REPL `read_line` collapses piped multi-line stdin. The REPL read a
1024-byte chunk, returned the first line, and left the rest in the buffer — then
on the next call it `blocking_read`-ed *again before checking the buffer*, hit
`Closed` (EOF) with the remaining lines still buffered, and `flush_buffer`
returned all of them as one mega-statement. Net effect: only the first SQL
statement in a piped script ran. Smoke pipes a multi-statement `smoke.sql`, so
this had to be fixed for any assertion past line 1.
Fix: serve a complete buffered line before reading more; only `blocking_read`
when no newline is buffered. `crates/ducklink-cli/src/lib.rs::read_line`.
(T-1 closed) fixed inline; interactive REPL unaffected, piped scripts now run
every statement.

Output convention that fell out: smoke runs in `.mode csv`, so each `SELECT`
emits a header line then its value. We alias each SELECT (`... AS apple`) so the
header doubles as a human label in `smoke.expected`. Good enough; no `.headers
off` exists in the CLI today.

### 2026-06-17 baseN (crate-backed pilot: base32 + bs58)

base32/base58 codecs over BLOB<->VARCHAR. Exercised the compat-registry crate
path: scaffolded with `--crate base32,bs58`; the registry's note for bs58
("default-features = false + ['alloc']") was emitted as a comment, and I applied
the documented feature flags by hand. Both crates built clean on wasm32-wasip2
first try — the seeded compat statuses held.

Surfaced the second (and worse) bug, caught only because baseN's decoders return
NULL on invalid input:

(T-2 new) core scalar-result writer leaves NULL rows valid over garbage. A scalar
returning `Duckvalue::Null` called `duckdb_validity_set_row_invalid` on the mask
from `duckdb_vector_get_validity` — but DuckDB allocates that mask lazily, so a
result vector that had only seen valid rows has a **NULL mask**, and
set_row_invalid on it is a silent no-op. The row stayed "valid" over
uninitialized vector data: `base58_decode('invalid…')` came back as a 4.5 MB
garbage blob instead of SQL NULL; INT64 and TEXT NULL returns were equally
broken. Fix: `duckdb_vector_ensure_validity_writable(vector)` then re-fetch the
mask before marking the row invalid (added the missing binding to
`libduckdb-sys`). `crates/ducklink-core/src/lib.rs::write_duckvalue_to_vector`.
(T-2 closed) fixed; verified NULL now propagates for blob, int64, and text
scalar results, and the sample-extension host test + standalone smoke still pass.

Why the isin pilot didn't catch it: every isin smoke input was a *valid* ISIN,
so no scalar ever returned NULL. (T-5 new) scaffold/smoke should nudge authors to
include a NULL-returning case — the whole class of "validity mask" bugs only
shows on a NULL result. A lint ("smoke.sql has no SELECT whose expected is
NULL") would have surfaced T-2 on the very first extension. Open.
