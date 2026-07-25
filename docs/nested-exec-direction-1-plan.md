# `duckdb:extension/nested-exec` — Direction 1 (standalone `ducklink` host) design memo

Status: **(b.1) SHIPPED but materially wrong for fieldbook** — see §7 for the
2026-07-25 wasmtime-reentrancy PoC findings and revised recommendation. Short
form: `wasmtime` permits store re-entry from a host callback that has
`Caller<'_, T>` in hand (memo §1.3 wall #2 REFUTED); the real blocker is the
ducklink bindgen dispatch path that lowers the store context to `&mut T` before
the callback runs. Option (a) is therefore the right long-term fix, not the
`create-sibling-connection` WIT change of option (c). Option (c) is also
blocked by the wasm-core-lives-in-a-different-repo constraint. Direction 2
(native DuckDB extension) remains the answer for entries that reference
loaded extensions (see `native-extension/ducklink` submodule commit `0ef7edf`).

## 1. Phase-1 investigation — how the wasm-core host is wired today

### 1.1 One core store, one guest resource table

`crates/ducklink-host/src/lib.rs:1194`:

```rust
struct CoreExecution {
    store: Store<CoreStoreState>,
    bindings: duckdb_core_bindings::Libduckdb,
}
```

The entire host owns exactly one `CoreExecution`, held in
`Arc<Mutex<CoreExecution>>` (`lib.rs:1360`, `1800`, `5617`). All connections are
`ResourceAny` handles inside that store's resource table — created by
`guest.call_open(...)` at `lib.rs:4583` / `call_open_with_config` at `lib.rs:4622`,
stashed in `Manager.connections` and in the shared
`current_connection: Arc<Mutex<Option<ResourceAny>>>` (`lib.rs:1361`, `1852`, `5620`).

### 1.2 What the core WIT already exports

`wit/core/duckdb-core.wit` (the `database` interface, lines 14-133) already exposes
`open`, `open-with-config`, `close`, `execute`, `open-stream`, `prepare`,
`query-arrow`, `create-appender`. A `Connection` resource exists. So a second
sibling connection **inside the same store** could in principle be created by
calling `open` a second time — DuckDB's `Database` object is shared across
`Connection`s at the C++ layer. **The WIT is not the blocker.**

### 1.3 The re-entrancy wall (this resolves the "wasmtime forbids" claim)

The definitive answer is in an existing comment at `lib.rs:5760-5766` (the
sibling `query` import's re-entrancy guard):

> RE-ENTRANCY GUARD: a table/scalar callback runs INSIDE the core query engine,
> which means the single shared `core` mutex is ALREADY held by the outer
> `call_execute` on the same thread AND the core wasm store is mid-call.
> Re-entering would self-deadlock (the std Mutex is non-reentrant) and, even
> past the lock, violate wasmtime store re-entrancy.

Two independent walls stack:

1. **`Arc<Mutex<CoreExecution>>` is non-reentrant.** The outer `call_execute`
   holds the guard (`Manager::with_core`, `lib.rs:5627-5633`); the callback fires
   from a *different* `Store<ExtensionStoreState>` and cannot reach the core store
   without re-locking. This is a straightforward deadlock.
2. **Even without the mutex, wasmtime's store-in-use check bars re-entry.** A
   `StoreContextMut<'_, T>` for the core store exists only on the outer stack
   frame that called `guest.call_execute`. The extension-callback host function
   receives `&mut ExtensionStoreState` (the *extension* store's state) — it has
   no path to the core store's `StoreContextMut`, and Rust's borrow rules
   prevent stashing that reference in an `Arc` because its lifetime is tied to
   the outer frame. wasmtime's runtime store-lock check backstops the borrow
   checker.

Wasmtime *does* allow a host function invoked BY store S to call back into
store S (that host function has `StoreContextMut<'_, T>` in hand). The problem
is architectural: the extension dispatch pipeline crosses store boundaries, and
by the time we reach `nested_exec`, we're several frames removed from the core
store's context. Threading that context through `ExtensionManager::dispatch_scalar`
/ `dispatch_scalar_batch` / `dispatch_table` (`lib.rs:2595`, `2656`, ~`2747`) and
into every extension `Store` invocation is a fundamental restructuring of the
callback plumbing — not a small change.

### 1.4 Direction-2's fix does not port over

Native `duckdb_connect(db, out)` returns a fresh connection object over the
existing `Database` pointer, needing no wasm re-entry. The wasm core has no analog:
its `Connection` is a wasm resource created inside the core store, and the mere
act of *creating* a second one requires re-entering the store.

## 2. Options ranked by cost

### (a) Same-store nested call — **INFEASIBLE**

Estimate: not achievable without a plumbing overhaul.

To make this work you would have to:

- Replace `Arc<Mutex<CoreExecution>>` with a reentrant / cell-based structure.
- Thread the core `StoreContextMut<'_, CoreStoreState>` down through
  `ExtensionManager::dispatch_scalar` (`lib.rs:2595`), the per-store extension
  invocation, and out to the extension's `nested_exec` host binding.
- Have wasmtime accept nested `call_execute` on the same store from a host
  function invoked BY that store (this is supported when you have the context
  in hand — but only if you can *get* the context in hand).

That is architecturally correct but roughly a two-week refactor across
`ducklink-host` and `ducklink-runtime`, with a real risk of collateral
regressions to autocomplete's `query` fallback and the callback dispatcher.
Not a one-pass change.

### (b) Second `CoreExecution` on the same DuckDB file — **feasible, ~1 week**

Cache one sibling `CoreExecution` per underlying database, lazily on first
`nested_exec`.

**What must be built:**

1. Record the primary's DB path when the CLI calls `open` (`lib.rs:4579`) /
   `open_with_config` (`lib.rs:4609`). Store on `Manager`.
2. Add `nested_core: OnceCell<Arc<Mutex<CoreExecution>>>` and
   `nested_connection: OnceCell<Arc<Mutex<Option<ResourceAny>>>>` to `Manager`.
3. On first `nested_exec`:
   - If primary DB is `:memory:` → return `Err("nested-exec: unsupported on
     in-memory databases; open a file-backed database")`. Two in-memory opens
     are independent DuckDB `Database` objects.
   - Otherwise `instantiate_core` (`lib.rs:5884`) against the primary's path
     with a fresh `WasiCtx`, fresh `ExtensionManager`, fresh TVM state, fresh
     resource table. Cache it.
   - `guest.call_open(store, Some(path))` on the sibling; cache the returned
     `ResourceAny`.
4. Route `nested_exec` through the sibling: try_lock is unnecessary (sibling
   store is idle from the primary's perspective).
5. Handle `close`: sibling must be closed at process exit.

**Real costs / risks not obvious from the description:**

- **Extension replication.** The primary loads extensions (fieldbook itself,
  spatial, delta, etc.) into the primary `ExtensionManager`. SQL run through
  the sibling references *extension-provided* functions (scalar / table /
  parser / storage). The sibling core does **not** have those extensions
  loaded. Any `nested_exec` SQL that touches an extension function will fail
  with `Catalog Error: function does not exist`. Fieldbook's own use case
  (`fieldbook_run` executing a stored entry) is precisely this: entries can
  reference any extension the user has loaded. **This is the killer.**

  Two sub-options:
  - **(b.1)** Document the limitation: sibling supports built-in DuckDB only.
    Probably too restrictive for fieldbook to be useful.
  - **(b.2)** Mirror extension loads to the sibling. Requires calling
    `ExtensionManager::load(name)` on the sibling every time it happens on
    the primary. That means intercepting every `LOAD`, wiring a second
    load pipeline (a second `thread::spawn` and callback registry per
    extension), and reconciling callback-handle namespaces across two
    `CallbackRegistry`s. This doubles the extension-load surface area.

- **First-call latency.** Instantiating a core wasm component takes seconds
  (component-compile cache mitigates warm; cold path is ~10× slower — see the
  `component-compile-cache` memory note). First `nested_exec` in a session
  pays this. Users won't like it but it is one-time.

- **Attached databases.** A user might `ATTACH 'other.db'` on the primary; the
  sibling has no knowledge. Would need to replay ATTACHes.

- **Transaction visibility.** Documented in `nested-exec.wit:16-21`: sibling
  sees the primary's *committed* state, not uncommitted mid-transaction
  changes. Two file opens: DuckDB's MVCC should handle this correctly, but
  concurrent writes across two `Database` objects to the same file need
  validation.

**Verdict.** (b.1) is straightforward (~2 days) but of limited use to fieldbook.
(b.2) — the version that actually solves the fieldbook use case — is closer to
a week and doubles the extension-load blast radius. Not appropriate for a
one-pass implementation without user sign-off on the trade-off.

### (c) Add `create-sibling-connection` (or similar) to the core WIT — **1-2 weeks**

Add an export to `wit/core/duckdb-core.wit` (`database` interface) that returns
a *sibling connection resource* the host can use from a nested wasm frame
without re-entering the outer `call_execute`. The wasm-core implementation
would internally call `duckdb_connect` on the shared `Database` and register
the new `Connection` resource in a way that allows the sibling call to bypass
the store-in-use check.

**WIT change (additive, backwards-compatible):**

```wit
interface database {
    // ... existing resources / funcs ...

    /// Returns a resource handle for a fresh sibling connection to the SAME
    /// underlying database. Safe to call from a host function invoked
    /// mid-`execute`; the returned handle can be passed to `execute-sibling`
    /// without re-entering the outer statement's store frame.
    open-sibling: func(conn: borrow<connection>) -> result<connection, string>;

    /// Runs `sql` on a sibling connection previously obtained from
    /// `open-sibling`. Implemented by a wasm-core entry point that dispatches
    /// on a separate wasmtime function so it can execute during an outer
    /// statement's callback.
    execute-sibling: func(conn: borrow<connection>, sql: string)
        -> result<query-result, duckerror>;
}
```

Because the existing WIT is only extended, every downstream component's frozen
`duckdb:extension` WIT copy is untouched — this is the same pattern as the
`nested-exec` interface itself (additive-only, opt-in per component).

**Host changes:**

- Reflect the two new bindings in `duckdb-core-bindings`.
- `CoreServices::nested_exec` calls `open-sibling` once (cache the resource in
  `Manager`), then `execute-sibling` per call. Both must go through a wasmtime
  entry point that does *not* take the store's outer lock — i.e. wasmtime
  needs a way to invoke a component function on a store that is already
  mid-call. This is the crux, and it is where option (c) may or may not be
  compilable depending on wasmtime's actual reentrancy story.

**Wasm-core changes (duckdb-wasm submodule):**

The C++ / wasm side must:

- Implement `open-sibling` as a synchronous call that creates a `Connection`
  over the current `DatabaseInstance` and adds it to the resource table.
- Implement `execute-sibling` in a way that the host can call it from inside a
  host-function callback. In practice this means the outer statement's
  `execute` needs to *release* the DuckDB `Connection`'s internal mutex before
  invoking the callback (DuckDB already does this for other callbacks
  including scalar UDFs), and the sibling call operates on a distinct
  `Connection` so there is no lock contention at the DuckDB level.

**Why this is the "right" long-term fix.** It aligns Direction 1 with
Direction 2's mental model: both create a sibling connection over the shared
database; only the mechanism (native pointer vs. wasm resource) differs. The
trust-model docstring at `nested-exec.wit:16-21` translates cleanly.

**Risks.**

- Wasmtime's store-in-use check may still forbid the nested invocation even
  with the WIT primitives in place. Requires a proof-of-concept before
  committing to the design.
- The duckdb-wasm submodule change is small on the C++ side but every
  in-tree `.wasm` core artifact must be re-built and its checksum re-pinned.
- Adds two exports to `duckdb:component/database`, so `libduckdb.wit`
  regenerates and every component that binds the core WIT is re-derived. If
  any consumer pinned exact bindings, they need a rebuild.

## 3. Recommendation

Implement **(c)**. It:

- Solves fieldbook's actual use case (any SQL that runs on the primary can run
  in a sibling — same catalog, same extensions).
- Keeps state single-sourced (one `ExtensionManager`, one connection pool).
- Extends the WIT additively so all frozen extension WITs stay untouched.
- Mirrors Direction 2's semantics.

But start with a **wasmtime reentrancy proof-of-concept** (est. 1 day) before
committing to the WIT/host/wasm-core changes. Concretely: build a two-function
test where a component's host import invokes a host callback that in turn calls
a *second* exported function on the same store — verify wasmtime accepts it
under the component model. If yes, (c) is straightforward. If no, escalate to
option (a)-style plumbing (thread `StoreContextMut` through the callback path).

Fall back to **(b.1)** — sibling core for built-in-only SQL, `:memory:` and
extension-function callers get a clear error — if (c) hits an unfixable
wasmtime barrier. It is worse than the current stub only in that it succeeds
sometimes but fails opaquely on extension-function references; therefore the
error path must be sharp: introspect the sibling's `catalog_error` and prefix
"nested-exec: sibling core does not have '<ext>' loaded; use Direction 2
(native ducklink extension) for cross-extension fieldbook entries".

## 4. Test plan (for whichever option is picked)

Once implementation exists:

1. Extend `crates/ducklink-runtime/src/extension.rs` tests
   (the `nested_exec_*` block starting at `extension.rs:5178`) to exercise the
   real host sink rather than the scripted one — probably via a small
   end-to-end test in `crates/ducklink-host/tests/` that loads a hand-built
   test component importing `duckdb:extension/nested-exec`.
2. Cover: (i) SELECT returns rows; (ii) INSERT returns `rows_affected`;
   (iii) syntax error surfaces the DuckDB error message; (iv) depth guard
   fires at `NESTED_EXEC_MAX_DEPTH + 1` (already covered at
   `extension.rs:5209`); (v) `:memory:` case — error if (b), works if (c).
3. Regression: run the existing autocomplete + dplyr + scalars smoke tests
   against the modified host; confirm the `query` re-entrancy fallback at
   `lib.rs:5769` still works.

## 5. Files that would change

Common: `crates/ducklink-host/src/lib.rs` (nested_exec impl at `5823`),
`crates/ducklink-host/tests/` (new test).

Option (b): additionally `Manager` struct + `open`/`open_with_config` (path
capture) + a new `instantiate_core_sibling` helper.

Option (c): additionally `wit/core/duckdb-core.wit` (two additive exports),
`duckdb-core-bindings` regen, duckdb-wasm submodule (C++ side of the two new
exports), pinned core `.wasm` artifact + checksum bump.

## 6. Cost summary

| Option | Cost | Solves fieldbook? | WIT change | Blast radius |
|--------|------|-------------------|-----------|--------------|
| (a) same-store re-entry | ~2 weeks (callback plumbing overhaul) | Yes | No | High |
| (b.1) sibling core, built-ins only | ~2 days | No (extension fns unavailable) | No | Low |
| (b.2) sibling core + extension mirroring | ~1 week | Yes, with duplicated ext state | No | Medium |
| (c) core-WIT sibling exports | ~1-2 weeks (incl. wasmtime PoC + wasm-core) | Yes | Yes (additive) | Medium |

## 7. PoC findings (2026-07-25) and refined plan

The §6 wasmtime-reentrancy PoC ran (see
`crates/ducklink-host/tests/reentrancy_poc.rs`). It probes the two "walls"
§1.3 stacks — Rust `Mutex` non-reentrancy and wasmtime's alleged store-in-use
check — as independent hypotheses.

### 7.1 Wall #1 (`std::sync::Mutex` non-reentrant): CONFIRMED

`wall1_std_mutex_is_non_reentrant` holds the outer guard on
`Arc<Mutex<u32>>` and calls `try_lock` from the same thread; the result is
`Err(TryLockError::WouldBlock)`, as documented. Any callback firing INSIDE
`Manager::with_core`'s guard (`crates/ducklink-host/src/lib.rs:4254`) cannot
re-acquire the primary core's mutex — self-deadlock on `lock()`, `WouldBlock`
on `try_lock()`. The existing RE-ENTRANCY GUARD comment at `lib.rs:5760` is
correct about *this* wall.

### 7.2 Wall #2 (wasmtime store "in use" check): REFUTED

`wall2_wasmtime_permits_reentry_from_host_callback_with_caller` builds a
plain wasm module exporting `run` and `sink`; `run` calls a host import `cb`;
the host `cb` uses its `Caller<'_, T>` to fetch the SAME instance's `sink`
export and calls it — RE-ENTERING the same store while `run` is mid-flight.
Wasmtime accepts it. The nested call runs to completion, control returns to
`cb`, which returns to `run`, which returns to the outer test frame. No trap,
no "store in use" error.

The `wall2_wasmtime_permits_calls_on_a_second_store_same_engine` companion
proves the "at most one exclusive borrow per store" invariant is per-store,
not per-engine — orthogonal evidence the constraint is a borrow-count
invariant, not a re-entrancy prohibition.

**Consequence.** The §1.3 claim that "wasmtime's runtime store-lock check
backstops the borrow checker" and forbids the nested call is factually
wrong. Wasmtime supports re-entry from a host callback provided the callback
has the store context in hand (a `Caller<'_, T>` — equivalently a
`StoreContextMut<'_, T>`). The wall is entirely on the ducklink side.

### 7.3 The real blocker: the ducklink dispatch path lowers away the store context

The bindgen `Host` traits ducklink installs via `add_to_linker(&mut linker,
|state| state)` (e.g. `core_callback_dispatch::add_to_linker` at
`lib.rs:6242`) receive `&mut CoreStoreState` — the store's *data*, not its
*context*. That is the store-data adapter shape wit-bindgen emits for
component-model imports. From `&mut CoreStoreState` there is no
`Caller<'_, T>` / `StoreContextMut<'_, T>` to be found — Rust's borrow
rules do not let you conjure one from the data reference, and no wasmtime
runtime path yields one either.

So option (a)'s "thread the store context through the callback path" is
NOT architecturally infeasible; it is *plumbing* — the exact plumbing the
bindgen-driven `HostWithStore` adapter API would provide (an existing
wasmtime feature, opt-in per interface). Cost is dominated by the size of
the callback-dispatch surface (`dispatch_scalar` / `dispatch_scalar_batch`
/ `dispatch_table` / `dispatch_aggregate*` / `dispatch_pragma` /
`dispatch_cast*`) that today all lower to `&mut CoreStoreState`, and by
propagating `StoreContextMut<'_, CoreStoreState>` across the ExtensionStore
boundary — which brings a second constraint: the extension `Store<ExtensionStoreState>`
must be entered from inside the core store's context frame without pinning
the core context, which means the "call into the extension" and "come back
to call into the core" steps have to interleave through a stackful control
flow. Doable, but a real refactor.

### 7.4 (b.1) failure mode reproduced end-to-end

Empirical reproducer for the task's success criterion — the shipped (b.1)
demonstrably fails the fieldbook end-to-end test:

```console
$ rm -f /tmp/poc.duckdb /tmp/poc.duckdb.wal
$ ducklink --extensions-dir <root>/artifacts/extensions --dir /tmp::/tmp \
  -- /tmp/poc.duckdb -c \
  "LOAD 'fieldbook'; \
   SELECT fieldbook_create('poc') AS created; \
   SELECT count(*) AS n FROM __fieldbook_books;"
...
[duckdb-core] macro registration failed (continuing): ... 'fieldbook_source': \
    Catalog Error: Table with name __fieldbook_entries does not exist!
| created |
+---------+
| true    |
internal error: Catalog Error: Table with name __fieldbook_books does not exist!
```

Two independent effects of (b.1) surface at once:

1. **Load-time bootstrap breaks.** Fieldbook's load hook uses `nested-exec`
   to `CREATE TABLE __fieldbook_books/__fieldbook_entries/__fieldbook_runs`.
   Under (b.1) the CREATEs land on the sibling — the primary's catalog never
   sees them, so the immediately-following `CREATE OR REPLACE MACRO
   fieldbook_source AS (SELECT ... FROM __fieldbook_entries ...)` on the
   primary fails with the exact `does not exist` we observe. The
   `is_extension_related_error` heuristic incorrectly PASSES this shape
   (it looks like a bare catalog miss, not "extension not loaded"), so no
   Direction-2 redirect fires and the load partially succeeds without the
   read macros.

2. **Runtime write invisibility.** `SELECT fieldbook_create('poc')` runs
   on the primary; its callback calls `nested-exec`, which writes to the
   sibling. `fieldbook_create` returns `true`. But the primary's
   `SELECT count(*) FROM __fieldbook_books` errors because the primary's
   catalog still doesn't have the table (the sibling's writes are file-level
   through DuckDB's WAL but the CATALOG is per-`Database` and does not
   auto-refresh across `Database` instances in the same process).

The (b.1) fallback is therefore not just "restrictive" — it is
**materially wrong** for fieldbook. The Direction-2 redirect only fires on
specific error shapes; the bootstrap failure is silently misclassified.

### 7.5 Blocker on option (c) as originally scoped

The memo's §5.(c) sketches `create-sibling-connection` / `execute-sibling`
as additive core-WIT exports. Two problems:

1. **Guest side lives in a different repo.** `ducklink_core.wasm` is built
   from `~/git/duckdb-wasm` (a separate git repo). The `.wasm` artifact is
   consumed by `crates/ducklink-host`; the C++/wasm-core side implementing
   the new exports is out of scope for changes made in *this* repo, per the
   task's file-touch restrictions. The WIT declaration is easy; the
   implementation ships in a different pipeline on a different cadence.

2. **The WIT change alone doesn't help.** Even if `create-sibling-connection`
   only opens a fresh `Connection` over the shared `DatabaseInstance`, calling
   it still needs a wasmtime entry — the wasm-core's `open` function runs
   inside its Store. During a callback the outer `guest.call_execute` holds
   the *entire* store's exclusive borrow (via the mutex, then via the
   wasmtime borrow). The only way to re-enter is by having the store context
   in hand — and if the callback already had it, `execute-sibling` would be
   redundant with plain `execute` on any resource.

Whatever the "right" wasm-core primitive is, the design MUST assume a
callback that already has `StoreContextMut<'_, CoreStoreState>`. Otherwise
the primitive can't be invoked.

### 7.6 Revised option matrix

| Option | Cost | Solves fieldbook? | Blocker |
|--------|------|-------------------|---------|
| (a) plumb `StoreContextMut` down the callback path | ~1-2 weeks (real refactor across `add_to_linker` sites + dispatch surface) | Yes — writes land on the primary connection | None; wasmtime supports the re-entry (§7.2 PoC) |
| (b.1) [SHIPPED] sibling core, built-ins only | Done | **No** — load-time bootstrap breaks silently (§7.4); redirect misclassifies | Design limitation |
| (b.2) sibling core + extension load mirroring | ~1 week + doubled extension-load blast radius | Partial; still no shared-catalog semantics | Not recommended (memo §2.(b)) |
| (c) core-WIT sibling exports | Unbounded — requires wasm-core repo changes (out of scope) + still needs (a)-style store-context plumbing to invoke | Yes IF (a) plumbing done anyway, in which case (c) is redundant | Out-of-scope wasm-core change |

### 7.7 Refined recommendation

1. **Fix the (b.1) misclassification NOW** (small, low-risk). The load-time
   fieldbook bootstrap failure produces `Catalog Error: Table ...
   __fieldbook_entries does not exist` at the *primary's* macro-registration
   step. `is_extension_related_error` should recognise "does not exist" on
   an identifier that looks like an internal table (`__` prefix) OR — more
   robustly — the fieldbook load hook itself should catch this and emit a
   clearer error that the user must run under the native `ducklink`
   extension for full functionality. This does not need any of (a)/(c);
   it just tightens the error surface.

2. **Pursue option (a) as the long-term fix**, not (c). The wasmtime PoC
   (§7.2) removes the "infeasible" objection. The work is a real refactor
   (thread `StoreContextMut<'_, CoreStoreState>` through the callback path;
   replace `Arc<Mutex<CoreExecution>>` with a shape that lets the callback
   reach the core context without deadlock, e.g. a thread-local pointer set
   for the duration of `with_core`), but it is well-scoped. Estimate:
   ~1-2 weeks focused work + a full regression pass on autocomplete /
   dplyr / scalars (which use the same callback path). Bigger tests to
   add: (i) the fieldbook success criterion from §7.4; (ii)
   `dispatch_scalar_batch` under high fan-out with nested-exec inside every
   row; (iii) `dispatch_table` streaming with nested-exec calls between
   `next()` iterations.

3. **Drop option (c) from consideration** unless the wasm-core repo grows a
   need for something like `create-sibling-connection` for reasons unrelated
   to Direction-1 nested-exec. It is redundant with (a) once (a) is done
   and cannot be shipped standalone (needs wasm-core cooperation this repo
   cannot force).

4. **Do NOT ship (b.2).** Reconfirmed after §7.4: mirroring extension loads
   to a sibling still leaves two disjoint catalogs, so the fieldbook
   read-macro DDL would still fail at registration time on the primary
   (macros reference tables the sibling owns).

### 7.8 Concrete next steps (out of scope for this PoC)

- Tighten `is_extension_related_error` OR the fieldbook load hook to
  surface the load-time bootstrap failure as an actionable
  "use Direction 2 for fieldbook on the standalone host" error. Small
  patch, safe to ship in the current minor.
- Cut a design ticket for option (a): "plumb `StoreContextMut<'_,
  CoreStoreState>` through the callback dispatch path so nested-exec runs
  against the primary connection." Include the PoC's `Caller<'_, T>`
  approach and the two-store cross-boundary interleaving problem (§7.3) as
  known hard parts.
- Keep the shipped (b.1) sibling core in place as the fallback for
  `nested-exec` from any *non*-fieldbook extension that only touches
  built-in DuckDB SQL and can tolerate the two-catalog semantic. This is
  where the WIT contract (`nested-exec.wit:16-21`) already promises
  "committed state only," so pure-read consumers are unaffected.
