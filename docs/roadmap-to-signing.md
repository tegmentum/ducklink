# DuckLink transparent `LOAD`: roadmap to a no-flag signing posture

DuckLink delivers transparent `LOAD <name>` on **stock, unmodified DuckDB** by
serving a per-name **shim** `.duckdb_extension` from a DuckLink repository; the
shim runs the multi-provider resolver in-process and loads the chosen provider
(native passthrough / wasm bridge / remote). See
`PLAN-multi-provider-extensions.md` and the native-extension shim
(`native-extension/ducklink/src/passthrough.rs` + `lib.rs`).

The one open constraint is **extension signing**. Stock DuckDB verifies every
loaded `.duckdb_extension` against a set of trusted public keys embedded in the
engine, unless unsigned extensions are explicitly allowed. DuckLink artifacts
are not signed by DuckDB's release key, so today they require the user to opt
into unsigned loading. This document is the path from that to a zero-friction,
no-flag end state.

## The three stages

### Stage 0 (now) - `-unsigned`

- The user launches DuckDB with `-unsigned` (CLI) or sets
  `allow_unsigned_extensions=true` at **database open** (it is a startup-only
  setting; it cannot be `SET` on a running database -
  `src/main/settings/custom_settings.cpp:197-201`).
- DuckLink shims carry a valid metadata footer with a **zeroed** 256-byte
  signature (`append_extension_metadata.py`). With `-unsigned`, stock DuckDB
  skips only the signature check; the footer parse + version/platform checks
  still run.
- Proven end-to-end on the official v1.5.4 CLI: `ducklink install aba; LOAD aba;`
  resolves the provider and runs `aba_validate` (native or wasm).
- Trade-off: `-unsigned` disables signature enforcement for *all* extensions in
  that session - a posture the operator chooses knowingly. This is the
  bootstrap, not the destination.

### Stage 1 - DuckLink key via `allow_community_extensions`

- DuckDB ships a **second** trust anchor for community extensions:
  `GetPublicKeys()` includes `community_public_keys[]` when
  `allow_community_extensions=true` (`extension_helper.cpp` /
  `extension_load.cpp:315-323`).
- DuckLink signs each shim (and each native provider artifact) with a key whose
  public half is in that community set, OR distributes through the channel that
  is already trusted under `allow_community_extensions`.
- The user then runs with `allow_community_extensions` (broad community trust)
  instead of `-unsigned` (no signature trust at all) - a strictly narrower,
  safer posture: only community-signed artifacts load, not arbitrary unsigned
  ones.
- Work: a DuckLink signing key + signing step in the generator (replace the
  zeroed signature with a real signature over `sha256(file up to -256)`); get
  the key into the community trust path.

### Stage 2 (end state) - DuckDB community-extension distribution (no flag)

- Publish DuckLink shims through DuckDB's **community-extensions** repository and
  signing pipeline, so they are trusted by a **default** stock DuckDB with **no
  flag at all**.
- At this point `INSTALL <name> FROM community; LOAD <name>;` (or DuckLink's own
  signed repo recognized by the community key) is fully transparent on an
  out-of-the-box DuckDB - the headline UX with zero security relaxation.
- Open question for the multi-provider model: a DuckLink shim is not a normal
  single-implementation extension; community-extension policy + review would
  need to accommodate "an extension that resolves and loads a provider." If that
  is not acceptable upstream, Stage 1 (DuckLink-key under
  `allow_community_extensions`) is the durable end state, and a one-time opt-in
  to the DuckLink trust anchor is the cost of the multi-provider capability.

## Independent of signing: the policy-override surface

A second stock-DuckDB constraint (orthogonal to signing): the **stable C
extension API has no runtime-setting registration** (no `duckdb_add_extension_
option`; only open-time `duckdb_set_config`), so a C-API/Rust shim **cannot make
`SET extension_provider=...` work** - stock DuckDB rejects unknown settings
("unrecognized configuration parameter"). Today DuckLink reads policy overrides
from the environment (`DUCKLINK_ALLOW_NATIVE`, `DUCKLINK_PROVIDER`,
`DUCKLINK_DENY`). The `SET`-based surface needs either a **C++ base extension**
(which can call `DBConfig::AddExtensionOption`) loaded before the shim, or DuckDB
exposing `add_extension_option` in the C extension API. Tracked separately from
signing.

## Summary

| Stage | User action | Trust posture | Status |
| --- | --- | --- | --- |
| 0 | `-unsigned` at open | no signature enforcement (session-wide) | done (proven) |
| 1 | `allow_community_extensions` + DuckLink key | only community-signed load | planned |
| 2 | none (default DuckDB) | default trust, no flag | goal (pending community-extension fit) |
