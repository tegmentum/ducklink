# qopt-component

Component-driven optimizer rule PoC. Registers an optimizer rule named
`qopt`; on any query whose flattened plan contains a GET on a table named
`optme`, the rule returns a `rewrite-query` directive that re-plans the
whole query to `SELECT 99 AS rewritten`.

## @5 status: dispatch surface is silently no-op'd

At `duckdb:extension@5.0.0` the `optimizer` interface (and its
`optimizer-dispatch` export) is DEPRECATED and no host consumes it:

- `crates/ducklink-runtime/src/extension.rs` still implements
  `extension_optimizer::Host::register_optimizer_rule` — the call succeeds
  and the registration is captured into a `pending_optimizers` buffer — but
  the buffer is never drained anymore. The core shim that used to wire a
  DuckDB `OptimizerExtension` around the captured rules is gone at @5.
- DuckDB's `OptimizerExtension` sits on the unstable internal C++ ABI with
  no equivalent in the stable C API, so there is no C-API path to re-home
  the dispatch to. This is the "no C-API equivalent" bucket in the @5 ADR
  (Decision 5(b)).

Net effect: `LOAD qopt;` succeeds and `qopt.load()` reports success, but
the rule is never offered a plan, so no rewrite ever fires. The scalars +
callbacks the component doesn't export are unaffected (it has none).

If DuckDB later publishes a stable-C-API optimizer-extension hook the
component can be re-homed onto that; until then this component is a
compile-only relic kept in-tree so the WIT interface + PoC stay
regression-tested.

## Build

```
cargo component build -p qopt-component --release --target wasm32-wasip2
```

Artifact: `target/wasm32-wasip1/release/qopt.wasm`
(cargo-component transforms `wasm32-wasip2` inputs into a `wasip1` core
module wrapped in a component — the output path is `wasip1`).
