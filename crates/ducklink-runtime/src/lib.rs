//! ducklink-runtime — the reusable wasm-component-loading engine.
//!
//! This crate is being extracted from `ducklink-host` so the same engine that
//! loads `duckdb:extension` wasm components can back two directions:
//!   1. the `ducklink` host, which runs DuckDB-compiled-to-wasm, and
//!   2. the native-DuckDB `ducklink` community extension (embeds wasmtime).
//!
//! Increment 1: the callback registry — maps a DuckDB-side function invocation
//! (by opaque handle) back to the owning wasm extension and its dispatcher.
//!
//! Increment 2: the `duckdb:extension` wasmtime bindings. The WIT world and its
//! generated host/guest types live here so both directions instantiate the same
//! component ABI; the host implements the `Host*` traits against its own store.
//!
//! Increment 4: the extension store-state + loaded-component instance (see the
//! [`extension`] module). The store-state implements the capability `Host*`
//! traits (capturing registrations into [`reg`]) and services config/logging
//! through an [`extension::ExtensionServices`] sink — the one direction-specific
//! seam.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use wasmtime::component::Component;
use wasmtime::Engine;

/// Whether `DUCKLINK_LOG=verbose` (case-insensitive) is set at process start.
/// Gates the per-registration `[extension-manager]` / `[extension-runtime:…]`
/// diagnostic prints so `LOAD ducklink` + `ducklink_load('<name>')` is silent
/// on the common path. Cached — the env var is read once and the value never
/// changes for the life of the process. Error paths and the tier-degradation
/// notices are NOT gated by this: they always print.
pub fn verbose_log_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("DUCKLINK_LOG")
            .and_then(|v| v.into_string().ok())
            .map(|s| s.eq_ignore_ascii_case("verbose"))
            .unwrap_or(false)
    })
}

/// Emit an `eprintln!`-shaped diagnostic only when `DUCKLINK_LOG=verbose`. Use
/// for per-registration and per-load traces that are useful when debugging
/// component wiring but pure noise for users on the happy path.
#[macro_export]
macro_rules! verbose_log {
    ($($arg:tt)*) => {
        if $crate::verbose_log_enabled() {
            eprintln!($($arg)*);
        }
    };
}

/// The AUTHORITATIVE, content-addressed `duckdb:extension` contract identity: a
/// **witcanon digest** — `sha256("witcanon:1" || canonical-WIT-bytes)` (hex),
/// the scheme from `compose-core::blobs::compute_wit_digest` in the
/// webassembly-component-orchestration framework (SPEC §4.1) — computed at build
/// time over the canonical `wit/duckdb-extension/*.wit` bytes (see `build.rs`).
///
/// This is the SOURCE OF TRUTH for the contract: a contract is identified by a
/// hash of its actual shape, not a hand-maintained version string. It changes iff
/// the WIT changes, interoperates with the framework's blob identity, and is what
/// `tooling/{gen,verify}-catalog.py` record + enforce per registry entry.
///
/// The runtime cannot recompute a *loaded* component's WIT digest (it can only
/// introspect the imported package @MAJOR — see [`component_contract_major`]), so
/// the runtime guard ([`check_component_contract`]) is the runtime-observable
/// PROXY for this identity; the digest is enforced at catalog-verify time.
include!(concat!(env!("OUT_DIR"), "/contract_digest.rs"));

/// The witcanon digest (hex) of the current canonical `duckdb:extension` WIT —
/// the authoritative content-addressed contract identity. Equals the value
/// `tooling/{gen,verify}-catalog.py` compute + record per registry entry.
pub fn contract_digest() -> &'static str {
    CONTRACT_DIGEST
}

/// The MAJOR version of the `duckdb:extension` WIT contract this host speaks.
///
/// This is the runtime-observable PROXY for the content-addressed contract
/// identity ([`CONTRACT_DIGEST`]): the host can only introspect a *loaded*
/// component's imported `duckdb:extension` package @MAJOR at runtime — it cannot
/// recompute the loaded component's WIT digest. Bump this when the canonical WIT
/// package id moves to a new major (e.g. `duckdb:extension@3.0.0`); the loader
/// guard ([`check_component_contract`]) rejects any component whose imported
/// package has a different major (or no version at all -- a legacy, pre-versioning
/// v1 component), so a mismatched component never instantiates and silently
/// marshals corrupted values. The AUTHORITATIVE check is the digest, enforced at
/// catalog-verify; this @MAJOR check is its runtime proxy.
///
/// v3 FREEZE (2026-06-28): the "v3 stabilization" completed the capability surface
/// (parser, general optimizer, window aggregate+frame, table-fn filter pushdown)
/// and landed it as the BREAKING `duckdb:extension@3.0.0` -- a DELIBERATE clean
/// break taken now (no external consumers yet), rejecting every @2.x component by
/// design rather than breaking users later. MAJOR is 3, the FROZEN BASELINE: all
/// ~188 components were rebuilt against it. Future growth is additive MINORS off
/// major-3 (opt-in worlds) + new types via the `complex()` escape hatch (no bump);
/// MAJOR-3 never bumps again. The authoritative contract identity is the
/// content-addressed canonical-WIT [`CONTRACT_DIGEST`] (compose:dynlink hash); this
/// @MAJOR is its runtime proxy. See docs/wit-freeze-policy.md.
///
/// major-4 (2026-06-28): the COLUMNAR dispatch break. The hot path
/// (call-scalar-batch-col / call-aggregate-col / call-cast-col) now crosses the
/// canonical ABI as typed `colvec`s (bulk memcpy per fixed-width column) instead
/// of the row-major `list<list<duckvalue>>` tagged-variant batch. This REMOVES
/// the major-3 row-major batch entries, so it is a true MAJOR break that rejects
/// every @3.x component by design -- taken now, in the no-users churn window
/// (measured 82-110x on the dispatch boundary). The cold singleton paths
/// (call-scalar/table/pragma/cast) stay row-major. See docs/v4-columnar-abi.md.
///
/// major-5 (2026-07-20): the TYPE-SURFACE break. Three coordinated changes land
/// as one MAJOR bump because each alone re-shapes the canonical ABI: S1 collapses
/// nested column payloads to an opaque `list<u8>` (with a `Complex(string)`
/// escape hatch on `logicaltype` so scalar/table/pragma paths can still name a
/// nested SQL type without a WIT shape); S2 promotes `decimal(width, scale)` to
/// a first-class typed columnar entry (out of the old fallback path); T2-1
/// removes residual HUGEINT / UHUGEINT surface (kept as row-major-only escape-
/// hatch complex types via S1). The @4.x columnar entries change shape (nested
/// column layout + decimal payload) and enum variants drop, so every @4.x
/// component is REJECTED by design. Taken in the no-users churn window; the
/// major-5 baseline is the new FROZEN reference and future growth is additive
/// MINORS. See docs/v5-type-surface.md.
pub const CONTRACT_MAJOR: u64 = 5;

/// The MINOR version of the `duckdb:extension` WIT contract this host speaks.
///
/// MINORs are ADDITIVE: a host at `MAJOR.minor` can load any component built at
/// `MAJOR.k` for `k <= minor` (forward-compat: the component imports a subset of
/// what the host provides). It CANNOT load a component built at a HIGHER minor —
/// that component imports interfaces (e.g. the 2.1 copy/secret/settings surface)
/// this host does not provide, so instantiation would fail with a cryptic
/// missing-import error. [`check_component_contract`] turns that into a friendly,
/// actionable message. Bump this in lockstep with each additive MINOR contract
/// bump (set back to 0 on a new MAJOR). Reset to 0 for the major-5 baseline.
pub const CONTRACT_MINOR: u64 = 0;

/// Full contract version string the host advertises (observability only; the
/// guard compares MAJOR.minor via [`CONTRACT_MAJOR`]/[`CONTRACT_MINOR`], and the
/// authoritative identity is the content-addressed [`CONTRACT_DIGEST`]).
pub const CONTRACT_VERSION: &str = "5.0.0";

/// The host's `duckdb:extension` contract version, for logging / a built-in.
/// This is the human-readable version; the authoritative content-addressed
/// identity is [`contract_digest`].
pub fn ducklink_contract_version() -> &'static str {
    CONTRACT_VERSION
}

/// The `duckdb:extension` WIT package name introspected by the runtime contract
/// guard. The shared [`datalink_contract`] guard is generic over the package
/// name (it also serves sqlink's `sqlink:wasm`), so we pin ducklink's here.
const CONTRACT_PACKAGE: &str = "duckdb:extension";

/// The `duckdb:extension` contract major a component targets, read from its
/// imported package ids. Returns:
///   - `Some(major)` if it imports `duckdb:extension/...@MAJOR.minor.patch`
///   - `None` if it imports the package UNVERSIONED (legacy pre-versioning v1)
///
/// Thin wrapper over the shared [`datalink_contract::component_contract_major`]
/// pinned to this host's [`CONTRACT_PACKAGE`].
pub fn component_contract_major(engine: &Engine, component: &Component) -> Option<u64> {
    datalink_contract::component_contract_major(engine, component, CONTRACT_PACKAGE)
}

/// The `duckdb:extension` contract `(major, minor)` a component targets, read
/// from its imported package ids. Returns:
///   - `Some((major, minor))` if it imports `duckdb:extension/...@MAJOR.MINOR.x`;
///     `minor` is the MAX minor across the package's interface imports (a 2.1
///     component imports the new interfaces `@2.1.x` -> minor 1; an existing 2.0
///     component imports everything `@2.0.x` -> minor 0).
///   - `None` if it imports the package UNVERSIONED (legacy pre-versioning).
///
/// Thin wrapper over the shared [`datalink_contract::component_contract_version`]
/// pinned to this host's [`CONTRACT_PACKAGE`] — the MINOR-granular companion to
/// [`component_contract_major`]. Lifted into `datalink-contract` so sqlink
/// inherits the same minor story from the one shared guard.
pub fn component_contract_version(engine: &Engine, component: &Component) -> Option<(u64, u64)> {
    datalink_contract::component_contract_version(engine, component, CONTRACT_PACKAGE)
}

/// Loader pre-check: reject a component whose `duckdb:extension` contract major
/// differs from this host's [`CONTRACT_MAJOR`] (or is unversioned/legacy) with a
/// clear, actionable error BEFORE instantiation. Wasmtime would itself reject a
/// truly mismatched component at instantiate time, but with a cryptic
/// type-mismatch trap; this gives the friendly message and explicitly catches the
/// unversioned-legacy case (which can silently marshal corrupted values because
/// the rich-types bump shifted enum discriminants).
///
/// The implementation is delegated to the shared [`datalink_contract`] crate
/// (also consumed by the sqlink host); only the package + host major are
/// ducklink-specific.
pub fn check_component_contract(
    engine: &Engine,
    component: &Component,
    extension_name: &str,
) -> wasmtime::Result<()> {
    // The shared minor-aware guard handles ALL cases with messages + behavior
    // preserved exactly: MAJOR-mismatch + unversioned/legacy (delegated to
    // datalink_contract::check_component_contract internally), plus the MINOR
    // gate -- same major but the component needs a HIGHER minor than this host
    // provides (it imports interfaces this host can't satisfy), rejected with a
    // friendly message BEFORE wasmtime fails at instantiate with a cryptic
    // missing-import error. A LOWER or equal minor is fine (additive
    // forward-compat: a 2.0 component on a 2.1 host still loads).
    let version = component_contract_version(engine, component);
    datalink_contract::check_component_version(
        version,
        CONTRACT_MAJOR,
        CONTRACT_MINOR,
        CONTRACT_PACKAGE,
        extension_name,
    )
    // datalink-contract returns anyhow::Result; map its error into the
    // wasmtime::Error this host's loader expects (the message is preserved).
    .map_err(|e| wasmtime::Error::msg(e.to_string()))
}

#[cfg(test)]
mod contract_guard_tests {
    use super::CONTRACT_MAJOR;

    // The component-introspection half (`component_contract_major`) needs a real
    // loaded Component; the smoke suite exercises it end-to-end by loading the
    // shipped @2 components. Here we pin the host-major decision the delegated
    // shared guard makes for ducklink's CONTRACT_MAJOR, so a major bump that
    // forgets to rebuild components stays caught.
    #[test]
    fn matching_major_loads_mismatch_and_unversioned_rejected() {
        // A @2 component matches this host -> Ok.
        assert!(datalink_contract::check_component_contract(
            Some(CONTRACT_MAJOR),
            CONTRACT_MAJOR,
            super::CONTRACT_PACKAGE,
            "ext",
        )
        .is_ok());

        // A @1 component is rejected with the friendly, actionable message.
        let mismatch = datalink_contract::check_component_contract(
            Some(1),
            CONTRACT_MAJOR,
            super::CONTRACT_PACKAGE,
            "ext",
        )
        .unwrap_err()
        .to_string();
        assert!(mismatch.contains("duckdb:extension contract 1.x"));
        assert!(mismatch.contains("ext"));

        // An unversioned/legacy component is rejected as such.
        let legacy = datalink_contract::check_component_contract(
            None,
            CONTRACT_MAJOR,
            super::CONTRACT_PACKAGE,
            "ext",
        )
        .unwrap_err()
        .to_string();
        assert!(legacy.contains("UNVERSIONED"));
        assert!(legacy.contains("duckdb:extension"));
    }

    // MINOR-gate completion (2.1.0): additive forward-compat within a major,
    // now exercised through the SHARED datalink_contract::check_component_version
    // path (the local check_contract_minor was lifted into datalink-contract).
    #[test]
    fn minor_gate_admits_equal_and_lower_rejects_higher() {
        use super::{CONTRACT_MINOR, CONTRACT_PACKAGE};
        let host_major = CONTRACT_MAJOR;
        let host_minor = CONTRACT_MINOR; // 1

        // Equal minor (2.1 component on a 2.1 host) -> Ok.
        assert!(datalink_contract::check_component_version(
            Some((host_major, host_minor)),
            host_major,
            host_minor,
            CONTRACT_PACKAGE,
            "ext",
        )
        .is_ok());

        // Lower minor (an existing 2.0 component on a 2.1 host) -> Ok
        // (additive forward-compat: it imports a subset of what the host provides).
        assert!(datalink_contract::check_component_version(
            Some((host_major, 0)),
            host_major,
            host_minor,
            CONTRACT_PACKAGE,
            "aba",
        )
        .is_ok());

        // Higher minor (a 2.1 component on a 2.0 host) -> rejected with the
        // friendly, actionable message BEFORE the cryptic instantiate failure.
        let err = datalink_contract::check_component_version(
            Some((2, 1)),
            2,
            0, // pretend this host only speaks 2.0
            CONTRACT_PACKAGE,
            "needs_copy",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs_copy"));
        assert!(err.contains("needs duckdb:extension contract >= 2.1"));
        assert!(err.contains("speaks 2.0"));

        // 2.2.0: the shared minor gate rejects a 2.2-component on a 2.1-host
        // (host_minor=1) with the friendly, actionable message -- the concrete
        // bump this PR lands. A 2.1-host cannot satisfy the 2.2 Items 6-7
        // imports, so the gate stops it BEFORE the cryptic instantiate failure.
        let err_22 = datalink_contract::check_component_version(
            Some((2, 2)),
            2,
            1, // pretend this host only speaks 2.1
            CONTRACT_PACKAGE,
            "needs_arrow",
        )
        .unwrap_err()
        .to_string();
        assert!(err_22.contains("needs_arrow"));
        assert!(err_22.contains("needs duckdb:extension contract >= 2.2"));
        assert!(err_22.contains("speaks 2.1"));

        // And the CURRENT 2.2 host (CONTRACT_MINOR=2) ADMITS a 2.2-component.
        assert!(datalink_contract::check_component_version(
            Some((CONTRACT_MAJOR, CONTRACT_MINOR)),
            CONTRACT_MAJOR,
            CONTRACT_MINOR,
            CONTRACT_PACKAGE,
            "arrow_ext",
        )
        .is_ok());

        // Legacy / no version -> the comprehensive shared guard rejects it as
        // legacy (the major-guard's concern, behavior preserved), not via the
        // minor gate.
        let legacy = datalink_contract::check_component_version(
            None,
            host_major,
            host_minor,
            CONTRACT_PACKAGE,
            "ext",
        )
        .unwrap_err()
        .to_string();
        assert!(legacy.contains("UNVERSIONED"));
    }
}

/// Native (wasmtime) host implementation of `compose:dynlink/linker` — the
/// resident, shared-provider "dlopen for components" bridge. Used by the
/// extension load path (so an `ml_kmeans`-style aggregate can reach the one
/// warmed pylon provider) and re-exported to `ducklink-host` for the dotcmd
/// path and the native proof tests.
/// Re-export the shared dynlink crate so the `impl_compose_dynlink_host!`
/// macro can reach `$crate::datalink_dynlink::impl_datalink_dynlink_host!`
/// from consumer crates.
pub use datalink_dynlink;

pub mod compose_dynlink;
pub use compose_dynlink::{ProviderPreopen, ProviderRegistry};

pub mod extension;
pub use extension::{
    add_extension_interfaces_to_linker, describe_runtime_logicaltype, load_component,
    load_component_with_dynlink, summarize_extopts, summarize_funcopts,
    summarize_registration_names, summarize_runtime_columns, summarize_runtime_funcargs,
    ConfigError, ExtensionInstance, ExtensionServices, ExtensionStoreState, LogEntry, LogField,
    LogLevel, PendingRegistrationsData,
};

/// The generated wasmtime bindings for the `duckdb:extension-host` world — the
/// capability surface a wasm extension component imports (register-scalar,
/// register-table, config, logging, catalog, files) plus the guest's exported
/// `load()` / `callback-dispatch`. Both the `ducklink` host and the native
/// `ducklink` DuckDB extension instantiate components against these bindings.
pub mod duckdb_extension_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension",
        require_store_data_send: true,
    });
}

/// Bindings for the storage-capable world (`duckdb-extension-storage`), which
/// additionally exports `storage-dispatch`. Only storage backend components
/// (e.g. sqlitewasm) satisfy this; the runtime builds these bindings lazily from
/// an already-loaded component instance so non-storage extensions (which don't
/// export storage-dispatch) still load against the base world above.
pub mod duckdb_extension_storage_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-storage",
        require_store_data_send: true,
    });
}

/// Bindings for the index-capable world (`duckdb-extension-index`), which
/// additionally exports `index-dispatch` (Item 3 / M2a custom index). Only
/// custom-index backend components (e.g. hnswfns) satisfy this; the runtime
/// builds these bindings lazily from an already-loaded component instance so
/// non-index extensions (which don't export index-dispatch) still load against
/// the base world above.
pub mod duckdb_extension_index_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-index",
        require_store_data_send: true,
    });
}

/// Bindings for the files-capable world (`duckdb-extension-files`), which
/// additionally exports `file-dispatch` (httpfs M2). Only files backend
/// components (e.g. webfs) satisfy this; the runtime builds these bindings
/// lazily from an already-loaded component instance so non-files extensions
/// (which don't export file-dispatch) still load against the base world above.
pub mod duckdb_extension_files_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-files",
        require_store_data_send: true,
    });
}

/// Bindings for the copy-capable world (`duckdb-extension-copy`, 2.1.0), which
/// additionally exports `copy-dispatch`. Only components that register a COPY
/// handler satisfy this; built lazily from an already-loaded instance.
pub mod duckdb_extension_copy_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-copy",
        require_store_data_send: true,
        // Reuse the base world's `types` so copy-dispatch exchanges the SAME
        // Duckvalue/Columndef/Duckerror the rest of the runtime uses -- no
        // per-world type conversion. NOTE: bump the @version here in lockstep
        // with the contract.
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the secret-capable world (`duckdb-extension-secret`, 2.1.0),
/// which additionally exports `secret-dispatch`. Only components that register a
/// secret type/provider satisfy this; built lazily from an already-loaded
/// instance.
pub mod duckdb_extension_secret_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-secret",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the writable-storage world (`duckdb-extension-storage-write`,
/// 2.1.0), which additionally exports `storage-write-dispatch` on top of the
/// read-only `storage-dispatch`. Only writable storage backends satisfy this;
/// built lazily from an already-loaded instance.
pub mod duckdb_extension_storage_write_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-storage-write",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the streaming-table world (`duckdb-extension-table-stream`,
/// 2.2.0, Item 6), which additionally exports `table-stream-dispatch`. Only
/// components that back a streaming/pushdown table function satisfy this; built
/// lazily from an already-loaded instance.
pub mod duckdb_extension_table_stream_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-table-stream",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the incremental-aggregate world (`duckdb-extension-aggregate-incr`,
/// 2.2.0, Item 6), which additionally exports `aggregate-incr-dispatch`. Only
/// components that back an incremental aggregate satisfy this; built lazily.
pub mod duckdb_extension_aggregate_incr_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-aggregate-incr",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the connection-lifecycle world (`duckdb-extension-conn`, 2.2.0,
/// Item 7), which additionally exports `conn-dispatch`. Only components that
/// subscribed to connection callbacks satisfy this; built lazily.
pub mod duckdb_extension_conn_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-conn",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the writable/glob files world (`duckdb-extension-file-write`,
/// 2.2.0, Item 7), which additionally exports `file-write-dispatch`. Only files
/// backends that support write/glob/stat satisfy this; built lazily.
pub mod duckdb_extension_file_write_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-file-write",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the general-index world (`duckdb-extension-index-write`, 2.2.0,
/// Item 7), which additionally exports `index-write-dispatch`. Only general
/// (non-ANN) index backends satisfy this; built lazily.
pub mod duckdb_extension_index_write_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-index-write",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the settings-callback world (`duckdb-extension-settings`, 2.2.0,
/// Item 7), which additionally exports `settings-dispatch`. Only components that
/// react to `SET <option>` satisfy this; built lazily.
pub mod duckdb_extension_settings_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-settings",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the parser-extension world (`duckdb-extension-parser`, 2.3.0 / v3),
/// which additionally exports `parser-dispatch`. Only components that register a
/// parser extension satisfy this; built lazily from an already-loaded instance and
/// driven by a DuckDB `ParserExtension` in the core shim.
pub mod duckdb_extension_parser_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-parser",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the general-optimizer world (`duckdb-extension-optimizer`, 2.3.0 /
/// v3), which additionally exports `optimizer-dispatch`. Only components that
/// register an optimizer rule satisfy this; built lazily and driven by a DuckDB
/// `OptimizerExtension` in the core shim (the generalized index-scan rule).
pub mod duckdb_extension_optimizer_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-optimizer",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the log-storage world (`duckdb-extension-log-storage`, 3.2.0),
/// which additionally exports `log-storage-dispatch`. Only components that back
/// a named log sink satisfy this; the runtime builds these bindings lazily from
/// an already-loaded instance. Class B parity with the stable
/// `duckdb_register_log_storage` C API.
pub mod duckdb_extension_log_storage_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-log-storage",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// Bindings for the arrow-ext world (`duckdb-extension-arrow-ext`, 4.0.0), which
/// additionally exports `arrow-ext-dispatch`. Only components that back a named
/// Arrow producer (registered via `arrow-ext.register-arrow-table`) satisfy this;
/// the runtime builds these bindings lazily from an already-loaded instance.
/// The 3-fn cursor (`call-arrow-open` / `-next` / `-close`) keeps state on the
/// guest — the host holds only an opaque cursor u32 and pulls row-vector
/// batches (`resultset`) until an empty resultset signals EOF; no arrow-rs types
/// cross the WIT boundary.
pub mod duckdb_extension_arrow_ext_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-arrow-ext",
        require_store_data_send: true,
        with: {
            "duckdb:extension/types@5.0.0": crate::duckdb_extension_bindings::duckdb::extension::types,
        },
    });
}

/// The kind of callback a handle dispatches to inside an extension component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackKind {
    Scalar,
    Table,
    Aggregate,
    Pragma,
    Cast,
    /// A named log-storage sink (Class B parity with the stable
    /// `duckdb_register_log_storage` C API). The dispatcher_handle routes
    /// every `write-log-entry` re-entry back to the owning component via
    /// [`ExtensionInstance::dispatch_write_log_entry`].
    LogStorage,
}

impl CallbackKind {
    pub fn describe(self) -> &'static str {
        match self {
            CallbackKind::Scalar => "scalar",
            CallbackKind::Table => "table",
            CallbackKind::Aggregate => "aggregate",
            CallbackKind::Pragma => "pragma",
            CallbackKind::Cast => "cast",
            CallbackKind::LogStorage => "log-storage",
        }
    }
}

/// Neutral registration model. A wasm extension's `load()` registers scalars,
/// tables, aggregates, macros, casts, etc. against the host's capability surface.
/// These types capture *what* was registered without referencing either the
/// wasm-DuckDB-core bindings (Direction 1) or the native DuckDB C API
/// (Direction 2), so the same capture path feeds both sinks. Each direction
/// converts these neutral records into its own loader/registration types.
pub mod reg {
    /// A DuckDB logical type, restricted to the value kinds the extension ABI
    /// currently exchanges. NOTE: no longer `Copy` -- the `Complex` escape-hatch
    /// arm carries an owned type-expression `String`, and the nested `List` /
    /// `Struct` / `Map` / `Array` arms (added @5.0.0) box or vec-own their
    /// element types.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum LogicalType {
        Boolean,
        Int64,
        Uint64,
        Float64,
        Text,
        Blob,
        Int32,
        Timestamp,
        Int8,
        Int16,
        Uint8,
        Uint16,
        Uint32,
        Float32,
        Date,
        Time,
        Timestamptz,
        /// S2 (major-5): DECIMAL now carries `width` (total digits) and
        /// `scale` (fractional digits) structurally, matching the WIT
        /// `decimal(decimalshape)` arm. Was fieldless on @4.
        Decimal { width: u8, scale: u8 },
        Interval,
        Uuid,
        /// T2-1 residual (major-5): 128-bit signed integer logical type.
        /// Fieldless -- the shape is fixed; values ride on
        /// `DuckValue::Hugeint`.
        Hugeint,
        /// T2-1 residual (major-5): 128-bit unsigned integer logical type.
        /// Fieldless -- companion to `Hugeint`.
        UHugeint,
        /// S1 (major-5): LIST logical type -- element type carried as a boxed
        /// nested `LogicalType` so the neutral shape can round-trip nested
        /// DuckDB type-expressions the WIT boundary still lowers through the
        /// `complex` escape hatch (wit-parser 0.251 forbids recursive VALUE
        /// types on the WIT side).
        List(Box<LogicalType>),
        /// S1 (major-5): STRUCT logical type -- ordered `(field-name, type)`
        /// entries. Field names preserve declaration order.
        Struct(Vec<(String, LogicalType)>),
        /// S1 (major-5): MAP logical type -- `(key-type, value-type)`.
        Map(Box<LogicalType>, Box<LogicalType>),
        /// S1 (major-5): fixed-size ARRAY logical type -- (size, element-type).
        Array(u32, Box<LogicalType>),
        /// ESCAPE-HATCH: a DuckDB type-expression string (e.g. "INTEGER[]",
        /// "STRUCT(a INTEGER, b VARCHAR)"). Carries an arbitrary declared return
        /// type the core resolves at registration time. Still used as the
        /// WIT-side representation for nested logical types, which the WIT
        /// `logicaltype` variant does not carry structurally in @5.
        Complex(String),
    }

    impl LogicalType {
        pub fn describe(&self) -> String {
            match self {
                LogicalType::Boolean => "BOOLEAN".to_string(),
                LogicalType::Int64 => "INT64".to_string(),
                LogicalType::Uint64 => "UINT64".to_string(),
                LogicalType::Float64 => "FLOAT64".to_string(),
                LogicalType::Text => "TEXT".to_string(),
                LogicalType::Blob => "BLOB".to_string(),
                LogicalType::Int32 => "INT32".to_string(),
                LogicalType::Timestamp => "TIMESTAMP".to_string(),
                LogicalType::Int8 => "INT8".to_string(),
                LogicalType::Int16 => "INT16".to_string(),
                LogicalType::Uint8 => "UINT8".to_string(),
                LogicalType::Uint16 => "UINT16".to_string(),
                LogicalType::Uint32 => "UINT32".to_string(),
                LogicalType::Float32 => "FLOAT32".to_string(),
                LogicalType::Date => "DATE".to_string(),
                LogicalType::Time => "TIME".to_string(),
                LogicalType::Timestamptz => "TIMESTAMPTZ".to_string(),
                LogicalType::Decimal { width, scale } => {
                    format!("DECIMAL({width}, {scale})")
                }
                LogicalType::Interval => "INTERVAL".to_string(),
                LogicalType::Uuid => "UUID".to_string(),
                LogicalType::Hugeint => "HUGEINT".to_string(),
                LogicalType::UHugeint => "UHUGEINT".to_string(),
                LogicalType::List(inner) => format!("LIST({})", inner.describe()),
                LogicalType::Struct(fields) => {
                    let parts: Vec<String> = fields
                        .iter()
                        .map(|(n, t)| format!("{n} {}", t.describe()))
                        .collect();
                    format!("STRUCT{{{}}}", parts.join(", "))
                }
                LogicalType::Map(k, v) => {
                    format!("MAP<{}, {}>", k.describe(), v.describe())
                }
                LogicalType::Array(size, inner) => {
                    format!("ARRAY({}, {size})", inner.describe())
                }
                LogicalType::Complex(expr) => expr.clone(),
            }
        }
    }

    /// A scalar/aggregate/table function argument. `name` is optional because
    /// positional arguments may be anonymous.
    #[derive(Clone, Debug)]
    pub struct FuncArg {
        pub name: Option<String>,
        pub logical: LogicalType,
    }

    /// A named output column of a table function.
    #[derive(Clone, Debug)]
    pub struct ColumnDef {
        pub name: String,
        pub logical: LogicalType,
    }

    /// Function attribute flags (mirrors `duckdb:extension/types.funcflags`).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct FuncFlags {
        pub deterministic: bool,
        pub commutative: bool,
        pub stateless: bool,
        pub side_effecting: bool,
        pub deprecated: bool,
    }

    impl FuncFlags {
        pub fn describe(self) -> String {
            let mut parts = Vec::new();
            if self.deterministic {
                parts.push("deterministic");
            }
            if self.commutative {
                parts.push("commutative");
            }
            if self.stateless {
                parts.push("stateless");
            }
            if self.side_effecting {
                parts.push("sideeffecting");
            }
            if self.deprecated {
                parts.push("deprecated");
            }
            if parts.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", parts.join(", "))
            }
        }
    }

    /// Optional metadata attached to a scalar/aggregate registration.
    #[derive(Clone, Debug)]
    pub struct FuncOpts {
        pub description: Option<String>,
        pub tags: Vec<String>,
        pub attributes: FuncFlags,
    }

    /// Optional metadata attached to a table-function registration.
    #[derive(Clone, Debug)]
    pub struct ExtOpts {
        pub description: Option<String>,
        pub tags: Vec<String>,
    }

    /// A scalar value exchanged across the callback boundary.
    #[derive(Clone, Debug)]
    pub enum DuckValue {
        Null,
        Boolean(bool),
        Int64(i64),
        Uint64(u64),
        Float64(f64),
        Text(String),
        Blob(Vec<u8>),
        Int32(i32),
        /// Microseconds since 1970-01-01 (DuckDB's TIMESTAMP representation).
        Timestamp(i64),
        Int8(i8),
        Int16(i16),
        Uint8(u8),
        Uint16(u16),
        Uint32(u32),
        Float32(f32),
        /// Days since 1970-01-01 (DuckDB's DATE representation).
        Date(i32),
        /// Microseconds since midnight (DuckDB's TIME representation).
        Time(i64),
        /// Microseconds since 1970-01-01 UTC (DuckDB's TIMESTAMP_TZ representation).
        Timestamptz(i64),
        /// HUGEINT-backed scaled decimal: value = (upper<<64 | lower), with
        /// `width` total digits and `scale` fractional digits.
        Decimal {
            lower: u64,
            upper: u64,
            width: u8,
            scale: u8,
        },
        /// INTERVAL: months + days + microseconds.
        Interval {
            months: i32,
            days: i32,
            micros: i64,
        },
        /// 128-bit UUID logical value, split into hi/lo halves.
        Uuid {
            hi: u64,
            lo: u64,
        },
        /// T2-1 residual (major-5): 128-bit signed integer scalar. Two
        /// halves matching the WIT `hugeintvalue` split -- the runtime
        /// reassembles the value via `((upper as i128) << 64 | lower as i128)`.
        Hugeint {
            lower: u64,
            upper: i64,
        },
        /// T2-1 residual (major-5): 128-bit unsigned integer scalar. Two
        /// halves matching the WIT `uhugeintvalue` split.
        UHugeint {
            lower: u64,
            upper: u64,
        },
        /// S1 (major-5): LIST scalar value -- ordered element values.
        List(Vec<DuckValue>),
        /// S1 (major-5): STRUCT scalar value -- ordered `(field-name, value)`
        /// entries.
        Struct(Vec<(String, DuckValue)>),
        /// S1 (major-5): MAP scalar value -- key/value pairs, in the order
        /// the guest emitted them.
        Map(Vec<(DuckValue, DuckValue)>),
        /// S1 (major-5): fixed-size ARRAY scalar value -- element values.
        Array(Vec<DuckValue>),
        /// ESCAPE-HATCH composite value: a DuckDB type-expression string plus the
        /// value rendered as JSON. The core reconstructs the real LIST/STRUCT vector
        /// from the JSON via the duckdb C vector API.
        Complex {
            type_expr: String,
            json: String,
        },
    }

    /// A scalar function registered by an extension.
    #[derive(Clone, Debug)]
    pub struct ScalarReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub returns: LogicalType,
        pub callback_handle: u32,
        pub options: Option<FuncOpts>,
    }

    /// A table function registered by an extension.
    #[derive(Clone, Debug)]
    pub struct TableReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub columns: Vec<ColumnDef>,
        pub callback_handle: u32,
        pub options: Option<ExtOpts>,
    }

    /// A storage / catalog backend registered by an extension. Keyed by an
    /// ATTACH `type_name` (e.g. "sqlite"); `callback_handle` routes every
    /// `storage-dispatch` call back to the owning component.
    #[derive(Clone, Debug)]
    pub struct StorageReg {
        pub extension: String,
        pub type_name: String,
        pub callback_handle: u32,
        pub options: Option<ExtOpts>,
    }

    /// A files backend registered by an extension (httpfs M2). The
    /// `callback_handle` routes every `file-dispatch` call back to the owning
    /// component. Only one files backend is active at a time.
    #[derive(Clone, Debug)]
    pub struct FilesReg {
        pub extension: String,
        pub callback_handle: u32,
    }

    /// A custom index TYPE registered by an extension (Item 3 / M2a). Declares an
    /// index type `type_name` (e.g. "wasm_hnsw") so `CREATE INDEX ... USING
    /// <type_name>` routes the build pipeline to the owning component's
    /// `index-dispatch` export. The build/search lifecycle is keyed by index NAME
    /// inside the component, so no per-type callback handle is needed.
    #[derive(Clone, Debug)]
    pub struct IndexReg {
        pub extension: String,
        pub type_name: String,
    }

    /// A collation registered by an extension (Item 2). Declares a collation
    /// `name` whose transform is the already-registered scalar `transform_scalar`
    /// (text -> sort-key text). The collation reuses the existing scalar dispatch
    /// entirely, so no callback handle is needed: the core looks the scalar up by
    /// name in the catalog when it registers the collation.
    #[derive(Clone, Debug)]
    pub struct CollationReg {
        pub extension: String,
        pub name: String,
        pub transform_scalar: String,
        pub combinable: bool,
    }

    /// A PRAGMA declared by an extension (Item 4). The user types
    /// `PRAGMA <name>(...)`; the core intercepts it, dispatches via the
    /// `callback_handle` (callback-dispatch.call-pragma), and the component
    /// RETURNS a SQL script (text) that the core then runs on the connection.
    /// No mid-callback re-entry into SQL, so no connection re-entrancy.
    #[derive(Clone, Debug)]
    pub struct PragmaReg {
        pub extension: String,
        pub name: String,
        pub callback_handle: u32,
    }

    /// An aggregate function registered by an extension.
    #[derive(Clone, Debug)]
    pub struct AggregateReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub returns: LogicalType,
        pub callback_handle: u32,
        pub options: Option<FuncOpts>,
    }

    /// A SQL macro registered by an extension.
    #[derive(Clone, Debug)]
    pub struct MacroReg {
        pub extension: String,
        pub schema: String,
        pub name: String,
        pub parameters: Vec<String>,
        pub definition_sql: String,
    }

    /// A replacement scan binding a set of file extensions to a table function.
    #[derive(Clone, Debug)]
    pub struct ReplacementScanReg {
        pub extension: String,
        pub extensions: Vec<String>,
        pub function_name: String,
    }

    /// A user-defined logical type alias over a physical type.
    #[derive(Clone, Debug)]
    pub struct LogicalTypeReg {
        pub extension: String,
        pub name: String,
        pub physical: String,
    }

    /// A cast between two named types, dispatched through a callback.
    ///
    /// `implicit_cost` (T2-4) mirrors DuckDB's cast-function implicit-conversion
    /// cost knob (`duckdb_cast_function_set_implicit_cost` in libduckdb-sys
    /// 1.10504.0). `None` means "use ducklink's default of 100" — DuckDB's C
    /// API default is actually `-1` (explicit-only, per cast_function-c.cpp:20
    /// CCastFunction::implicit_cast_cost), but ducklink treats an unset value
    /// as implicit-cost 100 to match the typical scalar-registration
    /// ergonomic. `Some(-1)` marks the cast explicit-only (parity with the C
    /// API's convention); any other `Some(v)` is a positive cost. The native
    /// reg_duckdb consolidator applies this at cast-function creation time.
    #[derive(Clone, Debug)]
    pub struct CastReg {
        pub extension: String,
        pub source: String,
        pub target: String,
        pub callback_handle: u32,
        pub implicit_cost: Option<i32>,
    }

    // --- 2.1.0 additive registrations ---

    /// A COPY handler registered by an extension (2.1.0, Item 1). Binds a file
    /// `extension` (e.g. "parquet") to a copy function; `function_handle` routes
    /// every `copy-dispatch` call back to the owning component.
    #[derive(Clone, Debug)]
    pub struct CopyHandlerReg {
        pub extension: String,
        pub file_extension: String,
        pub function_handle: u32,
    }

    /// A secret TYPE or PROVIDER registered by an extension (2.1.0, Item 2).
    /// `provider` is `None` for a bare type registration. `params` lists the
    /// accepted (name, redacted) keys (empty for a provider registration).
    /// `callback_handle` routes every `secret-dispatch` call.
    #[derive(Clone, Debug)]
    pub struct SecretReg {
        pub extension: String,
        pub type_name: String,
        pub provider: Option<String>,
        pub params: Vec<(String, bool)>,
        pub callback_handle: u32,
    }

    /// A configuration option declared by an extension (2.1.0, Item 3). Distinct
    /// from `config` reads: this DECLARES an option the core should expose.
    #[derive(Clone, Debug)]
    pub struct SettingReg {
        pub extension: String,
        pub name: String,
        pub description: String,
        /// One of "boolean" / "varchar" / "bigint" / "double".
        pub ty: String,
        pub default_value: Option<String>,
        /// "local" or "global".
        pub scope: String,
    }

    /// A TABLE macro declared by an extension (2.1.0, Item 5). The body is a SQL
    /// relation usable in the FROM clause.
    #[derive(Clone, Debug)]
    pub struct TableMacroReg {
        pub extension: String,
        pub schema: String,
        pub name: String,
        pub parameters: Vec<String>,
        pub body_sql: String,
    }

    /// A logical type registered over a full type-expression (2.1.0, Item 5),
    /// carrying modifiers (e.g. "DECIMAL(18,3)"). Rides the existing
    /// type-expression escape hatch -- no new `logicaltype` arm.
    #[derive(Clone, Debug)]
    pub struct ModifiedTypeReg {
        pub extension: String,
        pub name: String,
        pub type_expr: String,
    }

    /// An ENUM type registered by an extension (2.1.0, Item 5).
    #[derive(Clone, Debug)]
    pub struct EnumTypeReg {
        pub extension: String,
        pub name: String,
        pub members: Vec<String>,
    }

    /// A RICHER scalar function registered by an extension (2.2.0, Item 6) via
    /// `runtime-ext.register-scalar-ex`: varargs, (optionally named) args, and a
    /// NULL-handling mode. `varargs` is the declared trailing repeatable type
    /// (None = no varargs); `special_null` is true when the function is invoked on
    /// NULL inputs. `callback_handle` routes invocations.
    ///
    /// `volatile` controls whether the direction-specific sink marks the C API
    /// scalar as `duckdb_scalar_function_set_volatile` — the WIT `funcflags` does
    /// not expose a VOLATILE bit directly (yet), so the host derives it from the
    /// funcopts attributes: any scalar-ex whose attributes DON'T include
    /// `deterministic` is treated as VOLATILE (re-evaluated per row), matching
    /// DuckDB's IMMUTABLE vs VOLATILE semantics. Absent options default to
    /// non-volatile so the audit finding "treats every ex-path fn as VOLATILE by
    /// default" is closed.
    #[derive(Clone, Debug)]
    pub struct ScalarExReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub varargs: Option<LogicalType>,
        pub returns: LogicalType,
        pub special_null: bool,
        pub volatile: bool,
        pub callback_handle: u32,
        pub options: Option<FuncOpts>,
    }

    /// A connection-lifecycle subscription registered by an extension (2.2.0,
    /// Item 7). `on_opened`/`on_closed` mirror the requested `conn-events` flags;
    /// `callback_handle` routes every `conn-dispatch` notification.
    #[derive(Clone, Debug)]
    pub struct ConnCallbackReg {
        pub extension: String,
        pub on_opened: bool,
        pub on_closed: bool,
        pub callback_handle: u32,
    }

    /// A coordinate reference system registered by an extension (2.2.0, Item 7).
    #[derive(Clone, Debug)]
    pub struct CoordinateSystemReg {
        pub extension: String,
        pub auth_name: String,
        pub code: u32,
        pub wkt: String,
    }

    /// An Arrow table producer registered by an extension (2.2.0, Item 7).
    /// `callback_handle` routes the host's pull calls.
    #[derive(Clone, Debug)]
    pub struct ArrowTableReg {
        pub extension: String,
        pub name: String,
        pub columns: Vec<ColumnDef>,
        pub callback_handle: u32,
    }

    /// A text encoding registered by an extension (2.2.0, Item 7).
    #[derive(Clone, Debug)]
    pub struct EncodingReg {
        pub extension: String,
        pub name: String,
        pub aliases: Vec<String>,
        pub callback_handle: u32,
    }

    /// A compression codec registered by an extension (2.2.0, Item 7).
    #[derive(Clone, Debug)]
    pub struct CompressionReg {
        pub extension: String,
        pub name: String,
        pub file_extension: String,
        pub callback_handle: u32,
    }

    /// DEPRECATED (ducklink 5.0.0). Scheduled for removal at the next
    /// `duckdb:extension` major bump. No host consumes `ParserReg`s anymore —
    /// ducklink is C-API-only and DuckDB's `ParserExtension` is on the
    /// unstable internal C++ ABI. See ducklink v4.6.0 (advanced-tier removal).
    ///
    /// A parser extension registered by an extension (2.3.0 / v3). `callback_handle`
    /// routes every `parser-dispatch.call-parse` to the owning component. The core
    /// shim wires a DuckDB `ParserExtension` that forwards unrecognized statement
    /// text and applies the returned string->SQL rewrite.
    #[derive(Clone, Debug)]
    pub struct ParserReg {
        pub extension: String,
        pub name: String,
        pub callback_handle: u32,
    }

    /// DEPRECATED (ducklink 5.0.0). Scheduled for removal at the next
    /// `duckdb:extension` major bump. No host consumes `OptimizerReg`s anymore —
    /// ducklink is C-API-only and DuckDB's `OptimizerExtension` is on the
    /// unstable internal C++ ABI. See ducklink v4.6.0 (advanced-tier removal).
    ///
    /// A general optimizer rule registered by an extension (2.3.0 / v3).
    /// `callback_handle` routes every `optimizer-dispatch.call-optimize` to the
    /// owning component. The core shim wires a DuckDB `OptimizerExtension` that
    /// offers the flattened plan-shape and applies the returned rewrite directive.
    #[derive(Clone, Debug)]
    pub struct OptimizerReg {
        pub extension: String,
        pub rule_name: String,
        pub callback_handle: u32,
    }

    /// DEPRECATED (ducklink 5.0.0). Scheduled for removal at the next
    /// `duckdb:extension` major bump. No host wires filter-pushdown table
    /// functions anymore — ducklink is C-API-only and the streaming
    /// `TableFunction` with `filter_pushdown = true` is on the unstable
    /// internal C++ ABI. Components should register through
    /// `runtime.table-registry` and filter above the scan. See ducklink v4.6.0.
    ///
    /// A STREAMING + FILTER-PUSHDOWN-capable table function registered by an
    /// extension via the additive 3.1.0 `table-stream` interface (the first
    /// additive MINOR off the frozen major-3 baseline). Unlike [`TableReg`] (the
    /// whole-batch `runtime.table-registry` path), this opt-in marker tells the
    /// core to wire a C++ streaming `TableFunction` with `filter_pushdown = true`
    /// that pushes the conjunctive filter set down to the owning component's
    /// `table-stream-dispatch.call-table-open-filtered`. `callback_handle` routes
    /// every streaming dispatch call back to that component.
    #[derive(Clone, Debug)]
    pub struct FilterableTableReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub columns: Vec<ColumnDef>,
        pub callback_handle: u32,
    }
}

/// One registered callback: which extension owns it, the guest-side dispatcher
/// handle to invoke, and the function kind.
#[derive(Clone, Debug)]
pub struct CallbackEntry {
    /// The owning extension name as a refcounted slice. `Arc<str>` (rather than
    /// `String`) makes the per-row dispatch handoff a cheap atomic refcount bump
    /// instead of a heap allocation + copy, while still indexing the
    /// `HashMap<String, ...>` of loaded extensions via `Borrow<str>`.
    pub extension: Arc<str>,
    pub dispatcher_handle: u32,
    pub kind: CallbackKind,
    /// Weak handle to the owning `ExtensionInstance`, populated by the
    /// direction-specific loader via [`CallbackRegistry::link_extension_instance`]
    /// after the instance is wrapped in `Arc<Mutex<>>`. The dispatch prologue
    /// tries `.upgrade()` first — a lock-free path that skips the second
    /// HashMap lookup keyed on `extension`. Left as `Weak::new()` when
    /// unpopulated (e.g. the standalone `ducklink` host that owns its own
    /// instance map); callers that see `None` from `.upgrade()` fall back to
    /// their normal lookup path, so this field is a pure optimisation with a
    /// safe default.
    pub instance: Weak<Mutex<ExtensionInstance>>,
}

/// A cache-line-padded slot in [`CallbackRegistry::entries`]. Two threads
/// dispatching on DIFFERENT (but nearby) handles read `entries[a]` and
/// `entries[b]`; without padding those Options land within the same 64-byte
/// line on typical x86_64 / aarch64 and each read invalidates the other's
/// cache line — a 5%+ regression measured on the `parallel_cross_ext` bench
/// after switching to Vec-indexed storage. `align(64)` forces every slot
/// onto its own line so cross-instance dispatch scales cleanly.
#[repr(align(64))]
#[derive(Default)]
struct CallbackSlot(Option<CallbackEntry>);

/// Allocates stable host-side handles and maps them to `CallbackEntry`s. The
/// host hands a handle to DuckDB at registration; DuckDB passes it back on every
/// invocation, and the engine routes it to the owning component.
///
/// Handles are dense small ints starting at 1, monotonically increasing. The
/// backing store is a `Vec<CallbackSlot>` (each slot 64-byte aligned) indexed
/// by handle — a direct pointer offset instead of a HashMap hash + bucket
/// walk on every dispatch. Slot 0 is unused (handles start at 1) so the index
/// math is a straight cast. Removed callbacks leave `None` in place; the Vec
/// never shrinks, matching the "handles never rebind" contract loadable
/// extensions rely on.
#[derive(Default)]
pub struct CallbackRegistry {
    next_handle: u32,
    entries: Vec<CallbackSlot>,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        Self {
            next_handle: 1,
            entries: Vec::new(),
        }
    }

    /// Ensure `entries` covers `handle`, filling any gap with `None`.
    #[inline]
    fn ensure_slot(&mut self, handle: u32) {
        let idx = handle as usize;
        if self.entries.len() <= idx {
            self.entries
                .resize_with(idx + 1, CallbackSlot::default);
        }
    }

    pub fn allocate(&mut self, extension: &str, kind: CallbackKind, dispatcher_handle: u32) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        self.ensure_slot(handle);
        self.entries[handle as usize].0 = Some(CallbackEntry {
            extension: Arc::from(extension),
            dispatcher_handle,
            kind,
            // Filled in by [`link_extension_instance`] once the owning
            // ExtensionInstance is wrapped in Arc<Mutex<>> (the loader can't
            // do it here because that wrap happens AFTER load_component
            // returns, and allocations fire during load_component's setup).
            instance: Weak::new(),
        });
        verbose_log!(
            "[extension-manager] registered {} callback handle {} for '{}' (dispatcher={dispatcher_handle})",
            kind.describe(),
            handle,
            extension
        );
        handle
    }

    pub fn remove(&mut self, handle: u32) {
        let idx = handle as usize;
        if let Some(slot) = self.entries.get_mut(idx) {
            if let Some(entry) = slot.0.take() {
                verbose_log!(
                    "[extension-manager] released {} callback handle {} for '{}'",
                    entry.kind.describe(),
                    handle,
                    entry.extension
                );
            }
        }
    }

    pub fn remove_extension(&mut self, extension: &str) {
        let mut removed = 0usize;
        for slot in self.entries.iter_mut() {
            let matches = slot
                .0
                .as_ref()
                .is_some_and(|entry| &*entry.extension == extension);
            if matches {
                slot.0 = None;
                removed += 1;
            }
        }
        if removed > 0 {
            verbose_log!(
                "[extension-manager] purged {removed} callback handles after unloading '{}'",
                extension
            );
        }
    }

    pub fn get(&self, handle: u32) -> Option<CallbackEntry> {
        self.entries
            .get(handle as usize)
            .and_then(|slot| slot.0.clone())
    }

    /// Borrowing handle resolution for the dispatch hot path. Unlike [`get`],
    /// this does NOT clone the [`CallbackEntry`], so the per-row scalar path
    /// reads `dispatcher_handle` + `kind` and borrows the owning extension name
    /// with no allocation (the caller then refcount-bumps just the `Arc<str>`
    /// name it needs). The caller holds the registry lock for the duration of
    /// the borrow, which on the dispatch path is already the case.
    #[inline]
    pub fn resolve(&self, handle: u32) -> Option<&CallbackEntry> {
        self.entries
            .get(handle as usize)
            .and_then(|slot| slot.0.as_ref())
    }

    /// Like [`allocate`] but without the per-registration `eprintln!`. Used by
    /// benchmarks/tests that allocate many handles; the production `allocate`
    /// keeps its load-time log line.
    pub fn allocate_quiet(
        &mut self,
        extension: &str,
        kind: CallbackKind,
        dispatcher_handle: u32,
    ) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        self.ensure_slot(handle);
        self.entries[handle as usize].0 = Some(CallbackEntry {
            extension: Arc::from(extension),
            dispatcher_handle,
            kind,
            instance: Weak::new(),
        });
        handle
    }

    /// Populate the `Weak<Mutex<ExtensionInstance>>` on every callback entry
    /// owned by `extension_name`. Called by the direction-specific loader
    /// (`Engine2::load` in the DuckDB extension) immediately after the newly
    /// loaded [`ExtensionInstance`] is wrapped in `Arc<Mutex<>>`, so the
    /// dispatch prologue can subsequently upgrade the Weak in a single atomic
    /// load — skipping the second `HashMap<extension_name, ...>` lookup and
    /// the `Arc<str>` clone that would otherwise happen on every dispatch.
    ///
    /// Iterates the `entries` `Vec` once and clones the same `Weak` into
    /// every matching slot; typical component loads register a handful of
    /// callbacks (scalars + tables + aggregates), so the walk is cheap.
    ///
    /// Idempotent: safe to call repeatedly (e.g. on re-load) — every slot
    /// gets the latest weak. Slots for unrelated extensions are left alone,
    /// so mixed-extension loads compose cleanly.
    pub fn link_extension_instance(
        &mut self,
        extension_name: &str,
        instance: &Arc<Mutex<ExtensionInstance>>,
    ) {
        let weak = Arc::downgrade(instance);
        for slot in self.entries.iter_mut() {
            if let Some(entry) = slot.0.as_mut() {
                if &*entry.extension == extension_name {
                    entry.instance = weak.clone();
                }
            }
        }
    }
}
