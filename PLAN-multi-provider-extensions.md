# DuckLink Multi-Provider Extension Architecture

## Status

Draft (refined) — adds the two-contract model + conformance-as-resolution-gate,
and grounds the design in the providers already shipped (wasm-component, remote-quack).

## Authors

Tegmentum LLC

## Motivation

Traditional DuckDB extensions are native shared libraries tied to host OS, CPU
architecture, compiler ABI, and the DuckDB internal extension ABI version. This
forces rebuilds across releases, excludes browser/WASI/serverless environments,
imposes heavy CI on authors, and leaves the long tail of niche extensions unwritten.

DuckLink separates the **logical extension contract** from the **implementation
substrate**. The user interacts with a single extension interface; DuckLink
dynamically selects the best available implementation for the environment.

---

# Goals

- **G1 — Single user-facing interface.** `INSTALL spatial; LOAD spatial; SELECT st_buffer(geom,100);` is identical in every environment.
- **G2 — Multiple execution providers.** native / wasm-component / wasm-browser / remote / hardware-accelerated, all behind one logical extension.
- **G3 — Native performance when available.** Native users get native speed automatically (the *fast native passthrough*).
- **G4 — Universal deployability.** native DuckDB, browser, WASI, serverless, embedded, remote.
- **G5 — Portable extension development.** Ship a portable wasm implementation first; optimize with native/accelerated providers later, with no SQL changes.

# Non-Goals

DuckLink does **not** replace native extensions, force WebAssembly everywhere,
guarantee identical performance, or emulate unsupported hardware.

---

# Architecture

## Logical Extension

A logical extension (`spatial`) is an identity over **functions, types, table
functions, replacement scans, configuration, capabilities, version
requirements** — with **no implementation detail**. It is the unit the user
`LOAD`s and the unit the resolver resolves.

## The Two Contracts (the load-bearing refinement)

The original draft spoke of "the contract" as one thing. It is two, and keeping
them distinct is what makes the model sound. A provider must satisfy **both**:

### 1. The ABI contract — *how a provider plugs in*

This is substrate-specific:

| Provider kind | ABI contract |
| --- | --- |
| wasm-component / wasm-browser | the `duckdb:extension` **WIT** world + the **witcanon contract digest** (`sha256("witcanon:1" ‖ canonical-WIT)`), guarded by `datalink-contract` `@MAJOR` |
| native | DuckDB's native C/C++ extension ABI (`duckdb_create_scalar_function`, …) |
| remote-quack | the quack wire protocol (DuckDB-internal serialization over HTTP) |

The ABI contract is **not shared across provider kinds.** A native `.duckdb_extension`
does not speak the WIT; a wasm component does not speak the native ABI. The ABI
contract only certifies that a provider of *that kind* is loadable and dispatchable.

### 2. The semantic contract — *what a provider must behave like*

This is substrate-**independent** and is the actual product promise:

- identical function signatures
- identical type behavior and NULL handling
- identical determinism
- identical transaction semantics
- identical error conditions

Performance differences are acceptable. **Behavior differences are not.**

The semantic contract is **not** enforced by a shared ABI (there isn't one across
kinds). It is enforced **only** by the conformance suite (below). This reframes
conformance from "testing hygiene" to *the mechanism by which the semantic
contract exists at all*.

> **Key invariant:** the semantic contract is versioned by the **witcanon
> contract digest** of the logical extension's WIT (the wasm-baseline ABI doubles
> as the canonical signature definition). A contract-digest bump re-opens
> certification for *every* provider, including native.

## Provider Implementations

```
spatial
├── native-linux-x86_64        (native ABI)
├── native-macos-arm64         (native ABI)
├── native-avx512              (native ABI, accelerated)
├── wasm-component             (WIT ABI — the portable baseline; defines semantics)
├── wasm-browser               (WIT ABI, browser-safe)
└── remote-quack               (quack protocol)
```

The **wasm-component provider is privileged**: it is the portable baseline that
runs everywhere, and — because its WIT is the witcanon contract — it is the
**reference implementation that defines the semantics** all others are certified
against.

## Extension Manifest

```yaml
name: spatial
semantic_contract: witcanon:90fdc46a585c   # the logical-extension contract digest

providers:
  - id: wasm-component
    kind: wasm
    abi: duckdb:extension@2.0.0
    artifact: sha256:...                    # content-addressed
    conformance: { suite: spatial@2, passed: true, at: witcanon:90fdc46a585c }
    reference: true                         # defines semantics

  - id: native-linux-x86_64
    kind: native
    platform: { os: linux, arch: x86_64 }
    artifact: sha256:...
    trust: { signed_by: ..., attestation: ... }   # native load is a trust decision
    conformance: { suite: spatial@2, passed: true, at: witcanon:90fdc46a585c }

  - id: remote-quack
    kind: remote
    endpoint: quack:analytics.internal:9494
    conformance: { suite: spatial@2, passed: true, at: witcanon:90fdc46a585c }
```

Every provider carries a `conformance` record keyed to a `semantic_contract`
digest. The resolver treats a provider whose `conformance.at` ≠ the manifest's
`semantic_contract` as **uncertified** (see the gate).

---

# The Resolver (the spine)

When `LOAD spatial` runs, the resolver selects a provider. The algorithm:

```
candidates = providers(spatial)
  |> filter(p => p.conformance.passed && p.conformance.at == semantic_contract)   # THE HARD GATE
  |> filter(p => substrate_available(p))      # native: platform match + loader; wasm: runtime; remote: reachable
  |> filter(p => trusted(p, policy))          # native/remote: trust policy; wasm: sandboxed (always allowed)
  |> filter(p => not user_excluded(p))        # `SET extension_provider_deny = ...`
order by precedence(env, policy)              # default: native(trusted) > wasm-local > wasm-browser > remote
take first, else FAIL with a precise reason   # never silently downgrade past an exclusion
```

Properties that matter:

- **Conformance is a hard gate, not a tiebreaker.** An uncertified provider is
  *not a candidate*, even if it's the only native option. Correctness never loses
  to performance. This is the difference between "multi-provider" and "multi-bug."
- **Precedence is overridable.** `SET extension_provider = 'wasm-component'` (or a
  policy) forces a kind — for reproducibility, sandboxing, or debugging — even on a
  native host. Determinism-sensitive workloads can pin the reference provider.
- **Graceful degradation, not silent failure.** If the preferred provider is
  unavailable/untrusted, fall to the next *certified* candidate; if none remain,
  fail with the reason (which gate each provider missed), never a wrong answer.
- **Resolution is observable.** `PRAGMA extension_provider('spatial')` reports the
  chosen provider + why (the losing candidates and their rejection reason).

### What the resolver reuses (it is not built from scratch)

| Resolver concern | Existing machinery |
| --- | --- |
| resolve a provider by id/digest, instantiate-once-and-reuse | `datalink-dynlink` `ProviderRegistry` (`register_digest`/`resolve_by_digest`) + `ResidentBackend` |
| the `@MAJOR`/witcanon ABI check | `datalink-contract::check_component_contract` |
| per-environment runtime selection | WasmMachine `LaunchConfig.runtime` (the per-tool runtime swap, already shipped for Route A) |
| the wasm-component provider | **Route A** — the real shell `LOAD`ing resident `duckdb:extension` components (shipped) |
| the remote provider | **quack** client/server (shipped) |
| trust for native/remote | `datalink-contract` + the attestation/`std:attest` path |

The resolver is the *policy + candidate-filtering spine* over these substrates —
new code, but thin: it orchestrates parts that already exist.

---

# Conformance (the hard gate, expanded)

DuckLink ships a **provider-neutral conformance suite** per logical extension —
the generalization of today's per-extension `smoke.sql`. A suite is run against
*every* provider; passing certifies that provider **at the current
`semantic_contract` digest**.

- The suite is **SQL-level and provider-blind**: same queries, same expected
  results, run against native / wasm / browser / remote without modification.
- A suite must cover the semantic-contract surface that drifts between
  implementations: NULL propagation, error *conditions* (and ideally messages),
  overflow/edge values, float determinism, and ordering/aggregation stability.
- A provider's `conformance` record is **(suite-version, contract-digest,
  pass/fail)**. The resolver admits only providers certified at the live digest.
- A **contract-digest bump re-opens certification for all providers** — the
  wasm-baseline WIT changing *is* the semantic surface changing, so every
  non-reference provider must re-certify before it's resolvable again.

Conformance is therefore the project's correctness backbone: it is simultaneously
the test suite, the semantic-contract definition, and the resolver's admission
criterion.

---

# Performance Model

- **Portable baseline (wasm-component).** Prioritizes compatibility, portability,
  safety, sandboxing. **Defines the semantics** (the reference provider).
- **Accelerated providers (native / SIMD / GPU / TPU / arch-specific).** May use
  any implementation strategy but **must pass conformance at the live contract**
  to be resolvable. Acceleration is admitted only after correctness is certified.

---

# The Native Passthrough (honest treatment)

The headline capability — native users get native speed transparently — with
three things stated plainly:

1. **It re-introduces the platform/ABI/rebuild burden the wasm model escapes —
   for that provider only.** That is acceptable *because* of graceful degradation:
   native is the opt-in accelerator; the wasm baseline always works. An author who
   ships only wasm loses nothing; an author who adds native gains speed where the
   platform matches.
2. **Loading a native `.duckdb_extension` is a trust decision.** "Trusted native"
   needs a concrete policy — signature/attestation via `datalink-contract` /
   `std:attest`, an allowlist, or `SET allow_native_providers = false` to forbid
   it entirely (the sandbox-preferred default for untrusted environments).
3. **Precedence must be overridable** so a user can force the wasm provider on a
   native host for reproducibility/sandboxing (`SET extension_provider = 'wasm-component'`).

---

# Development Lifecycle

1. **Ship wasm-component.** Works everywhere; *defines the semantics* (becomes the
   reference); passes its own conformance suite by construction.
2. **Adoption + reported bottlenecks.**
3. **Ship native providers** (`native-linux-x86_64`, …). Each must pass
   conformance at the live contract before the resolver will prefer it. Native
   users get acceleration; no SQL changes.
4. **Specialized providers** (`avx512`, `cuda`, `metal`, `remote-quack`). Same
   gate. The resolver picks the optimal *certified* implementation per environment.

---

# What Exists vs What's New

**Exists / shipped (this design is the roof over it):**
- wasm-component provider — **Route A** (real shell + resident `duckdb:extension` dispatch).
- remote-quack provider — **quack** client/server.
- wasm-browser path — jco + `@tegmentum/wasi-polyfill` + JSPI.
- the ABI contract + guard — witcanon digest + `datalink-contract` `@MAJOR`.
- resolve/instantiate/reuse — `datalink-dynlink`.
- per-environment runtime selection — WasmMachine `LaunchConfig.runtime`.
- a per-extension smoke test — the conformance suite's seed (`smoke.sql`).

**New (in dependency order):**
1. **The resolver spine** — candidate filtering (conformance gate + availability +
   trust + exclusion) + precedence + observability (`PRAGMA extension_provider`).
2. **The multi-provider manifest** — generalize `registry/index.json` from one
   artifact per extension to `providers[]` with `kind/abi/platform/trust/conformance`.
3. **Provider-neutral conformance** — promote `smoke.sql` to a cross-provider suite
   keyed on the `semantic_contract` digest; emit `conformance` records.
4. **The native passthrough** — the native-provider loader + the trust policy
   (gated on 1 + 3).

---

# Design Principle

DuckLink treats implementations as interchangeable providers behind a stable
**semantic** contract (certified by conformance), plugged in via substrate-specific
**ABI** contracts. Users choose capabilities; DuckLink chooses execution strategy;
performance is an implementation detail; **semantics — certified, not assumed — are
the product.**
