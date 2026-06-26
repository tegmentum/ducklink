# Assessment: where else to adopt the compose orchestration framework

`compose:dynlink` has landed end-to-end (native host + browser + the pylon
use case), the framework (`~/git/webassembly-component-orchestration`,
`sys:compose@1.0.0`) is now public, and sqlink already runs on it. This assesses
where ELSE ducklink should adopt it — selectively, where it adds unique value,
not a wholesale migration of working bespoke code.

## Calibration — what the compose:dynlink adoption taught us

These learnings set the cost of each candidate below:

1. **wasmtime alignment is the prerequisite — DONE.** ducklink + the framework
   are both on wasmtime 46.0.1 now. Any crate-level reuse of the wasmtime-bound
   host libs needs this; it's paid.
2. **Adoption splits three ways, with very different costs:**
   - **Spec/format adoption** — adopt the framework's *scheme* (the `witcanon:1`
     digest, canonical-CBOR identity, the `PlanV1` shape). Often best done by
     reimplementing the small, stable scheme (we reimplemented `witcanon` in 3
     lines rather than dep `compose-core`, to avoid heavy transitive deps) OR by
     depending on **`compose-core`, which is wasmtime-FREE** so it carries no
     version coupling. **Cheapest, interoperable.**
   - **Crate reuse (wasmtime-bound)** — dep `compose-host-wasmtime`. Coupled to
     wasmtime + to the framework's `DynState` store type, so it usually can't be
     dropped into ducklink's own store. **Medium-heavy.**
   - **Mirror** — reimplement the host trait over ducklink's store (what
     `compose:dynlink/linker` did, because `add_to_linker` is bound to
     `DynState`). **The realistic shape for host-side capabilities.**
3. **Keep bespoke where it works and the framework adds little.** The framework's
   value is *content-addressed reproducibility, declarative composition, trust/
   attestation, and cross-project consistency* — not replacing a working loader or
   a Python smoke runner.

## Candidate inventory + verdicts

| ducklink bespoke | framework capability | adoption type | value | cost | verdict |
|---|---|---|---|---|---|
| Contract guard (`@MAJOR`) | canonical-WIT identity (`witcanon`) | spec | high | — | **DONE** this session (`8d13d3c`) |
| Registry digests / component identity | `compose-core` `compute_digest` / blobs (content-addressed) | spec / `compose-core` dep | high | low | **ADOPT (Phase 1)** — unify on the content-addressed identity already used for the contract |
| `registry/builds.json` embedding tracking | `sys:compose` `PlanV1` (a plan *is* "what composes into a build") + `compute_plan_digest` | `compose-core` dep | med-high | med | **EVALUATE (Phase 1)** — `PlanV1` is real + conformance-tested; a bundle becomes a content-addressed plan |
| `wac` composition (postgis/mobilitydb/spatialproj) | `sys:compose` plan → exec (declarative, reproducible) | crate/exec | high | med-high | **ADOPT (Phase 2)** — hand-`wac` is fragile (we hit spatialproj/postgis re-compose pain); declarative plans + digests fix it |
| Component compile cache | `emit` `check_cache` + blobs | `compose-core` dep | med | med | **DEFER** — the bespoke cache works; revisit if Phase 1 blobs make it free |
| Extension signing toolchain (native ext) | `std:attest` + `trust-backends` (8 impls) + `std:secrets` | crate/spec | med | med | **CONSIDER (Phase 3)** — real value for the signed native-ext distribution; bounded (one artifact path) |
| Host component loading + capability injection | `compose:host` runner/invoker | mirror | low-med | high | **KEEP bespoke** — big migration, store-coupled, and we already extended this path with `compose:dynlink`; the framework adds little here |
| `DUCKLINK_NETWORK_GRANT` capability grants | SPEC §7 policy/capabilities | spec/mirror | low-med | med | **DEFER** — works; adopt the policy model only if Phase 2 plans pull it in |
| `tooling/smoke.py` | `compose:host/smoke` | — | low | med | **KEEP bespoke** — the Python smoke runner is fine; no real gain |
| Host logging | `std:metrics` + `std:audit` + events | spec | low | med | **SKIP for now** — nice-to-have, not load-bearing |

## Recommended roadmap (selective, value-ordered)

- **Phase 1 — content-addressed identity (low cost, high consistency).** Extend
  the `witcanon` contract-digest work to a full content-addressed identity via
  `compose-core` (it's wasmtime-free, safe to dep): record each component's
  content digest (`compute_digest`) in `registry/index.json` next to the
  `wit_contract` digest; `verify-catalog` already enforces the contract digest, so
  add the artifact digest. This unifies ducklink's identity with the framework's
  (and with sqlink) for ~no risk, and is the substrate Phase 2 needs.
- **Phase 2 — declarative composition via `PlanV1`/exec.** Replace the hand-rolled
  `wac plug` recipes (postgis/mobilitydb/spatialproj — which already bit us with
  stale/mismatched compositions) with `sys:compose` plans: each composed artifact
  is a content-addressed `PlanV1` (id→digest graph) executed by the framework. The
  payoff is reproducibility + a single composition substrate shared with sqlink.
  This is the highest-leverage non-trivial adoption.
- **Phase 3 — trust/attestation for the signed native extension.** Wire the native
  `ducklink` extension's signing/distribution onto `std:attest` + `trust-backends`
  + `std:secrets` so loadable artifacts carry verifiable provenance — the
  framework's differentiator over a bare metadata footer.
- **Hold / keep bespoke.** The host load path (just extended with
  `compose:dynlink`), the compile cache, the smoke runner, host logging, and the
  network-grant model — the bespoke versions work and the framework adds little;
  revisit only if a phase above pulls them in for free.

## Cross-project note

sqlink already consumes `compose:dynlink` (its own bespoke impl on wasmtime 45);
ducklink mirrors it on 46. With the framework public, the end state is **all three
projects depending on the same published `compose-core` (identity/plan/blobs/
trust)** and each mirroring the wasmtime-bound host bits over its own store. The
guard-consolidation decision (do not maintain two identity schemes) generalizes:
adopt the framework's *formats/specs* widely (cheap, interoperable), reuse its
*wasmtime-free crates* where useful, and mirror only the store-coupled host traits.

## Verification per phase

- Phase 1: registry carries both digests; `verify-catalog` enforces both; a
  perturbed artifact/WIT is caught; smoke unaffected.
- Phase 2: a postgis/mobilitydb composition is expressed as a `PlanV1`, executes to
  a byte-identical artifact, and its digest is stable + recorded; the smoke + the
  bridges still work.
- Phase 3: a signed native-ext artifact verifies through `trust-backends`; an
  unsigned/tampered one is rejected.
