//! Phase D: per-sub-extension `compose:dynlink` bridge + composed-provider
//! loader for `ducklink-host`. Mirrors
//! `datafission::df-plugin-loader::UnifiedPluginLoader::materialize_sub_ext_provider`
//! but wires into ducklink's `ProviderRegistry` and its `LOAD <ext>` path.
//!
//! Data flow at `LOAD postgis_core`:
//!
//!   1. `ExtensionManager::ensure_extension_loaded` checks
//!      [`SubExtLoader::has_bridge`] for the requested name; if present it
//!      takes the sub-ext branch instead of the flat
//!      `<extensions-dir>/<name>.wasm` resolution.
//!   2. [`SubExtLoader::materialize_sub_ext_provider`] parses the plan JSON,
//!      resolves any zero-digest components through the
//!      derived-from chain (recursing on this same method), composes via
//!      `compose_core::emit::EmitHandler`, writes the composed bytes to a
//!      stable file under `<blob_cas>/composed-providers/<sub_ext>-composed.wasm`,
//!      and registers under `sub_ext_provider_id(sub_ext)` in the
//!      process-global `ProviderRegistry`. Idempotent — a re-issued LOAD is a
//!      no-op after first hit.
//!   3. The caller then loads the bridge component's path
//!      (`sub_ext_bridge_paths[name]`) through ducklink's normal
//!      `load_component_with_dynlink` path; the bridge's `resolve-by-id`
//!      import routes to the newly-registered composed provider.
//!
//! Config comes from four env vars — parallel to `DUCKLINK_PROVIDERS`:
//!
//! ```text
//! DUCKLINK_SUB_EXT_PLANS=<name>=<path>:<name>=<path>...
//! DUCKLINK_SUB_EXT_BRIDGES=<name>=<path>:<name>=<path>...
//! DUCKLINK_SUB_EXT_DERIVED=<component-id>=<upstream-sub-ext>:<component-id>=<upstream-sub-ext>...
//! DUCKLINK_SUB_EXT_PREBUILT=<name>=<path>:<name>=<path>...
//! ```
//!
//! `DUCKLINK_SUB_EXT_PREBUILT` (short-circuit path — Phase D / Option B):
//! when a sub-ext has a prebuilt, self-contained composed wasm on disk
//! (e.g. the ~126 MB `postgis-composed.wasm` monolith produced by
//! `postgis-wasm/scripts/compose.sh`), register that wasm directly under
//! `<sub_ext>-composed` and skip the plan-driven compose entirely. Used
//! when the plan-driven sub-composed artifact would leak unresolved
//! sibling-wasm imports (see recon: 19 imports on `postgis-core-composed`
//! because `postgis_wasm.wasm` statically imports all 30+ external
//! instances). A prebuilt entry takes precedence over a plan entry for
//! the same sub-ext.
//!
//! `:` is the entry separator (matches datafission's convention and dodges
//! the `,` that DuckDB users habitually stuff into env vars for other reasons).
//!
//! Kept in its own module so the massive `lib.rs` doesn't grow further and
//! so the API surface (`SubExtLoader`, `SubExtError`, `sub_ext_provider_id`)
//! is unit-testable without dragging the whole `ExtensionManager` in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use compose_core::blobs::BlobStore;
use compose_core::emit::EmitHandler;
use compose_core::events::EventCollector;
use compose_core::host::SystemClock;
use compose_core::types::PlanV1;

use crate::ProviderRegistry;

/// Canonical provider id under which a sub-extension's composed backend
/// registers with the process-global `ProviderRegistry`. A bridge component
/// resolves through this id via `compose:dynlink/linker.resolve-by-id`.
/// Stable across sessions; identical to the datafission convention.
pub fn sub_ext_provider_id(sub_ext: &str) -> String {
    format!("{sub_ext}-composed")
}

/// Placeholder-digest predicate: a component whose real bytes come from
/// composing an upstream sub-ext's plan is authored with an all-zero
/// digest so plan JSON is byte-stable across regenerations. The loader
/// resolves such components via [`SubExtLoader::sub_ext_derived_from`].
fn is_zero_digest(digest: &[u8]) -> bool {
    !digest.is_empty() && digest.iter().all(|b| *b == 0)
}

/// compose-core's build-tool blob-size ceiling (1 GiB). Inlined so this
/// module doesn't drag in a limits import.
const COMPOSE_MAX_BLOB_SIZE: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum SubExtError {
    #[error(
        "no plan registered for sub-extension '{sub_ext}' — populate DUCKLINK_SUB_EXT_PLANS or SubExtLoader::sub_ext_plan_paths"
    )]
    PlanMissing { sub_ext: String },

    #[error(
        "no bridge registered for sub-extension '{sub_ext}' — populate DUCKLINK_SUB_EXT_BRIDGES or SubExtLoader::sub_ext_bridge_paths"
    )]
    BridgeMissing { sub_ext: String },

    #[error("failed to materialize sub-extension '{sub_ext}' from plan {plan:?}: {reason}")]
    Materialization {
        sub_ext: String,
        plan: PathBuf,
        reason: String,
    },

    #[error("failed to register composed provider '{id}' from {path:?}: {reason}")]
    ProviderRegistration {
        id: String,
        path: PathBuf,
        reason: String,
    },
}

impl SubExtError {
    fn mat(sub_ext: &str, plan: &Path, reason: impl Into<String>) -> Self {
        Self::Materialization {
            sub_ext: sub_ext.to_string(),
            plan: plan.to_path_buf(),
            reason: reason.into(),
        }
    }
}

/// Owns the sub-ext ↔ (plan, bridge) maps and the compose-core CAS/cache
/// roots. Lives inside `ExtensionManager` but is a self-contained,
/// unit-testable unit — the constructor takes only what materialization
/// needs (an engine-bound `ProviderRegistry` + two dirs), not the whole
/// LOAD-path state.
pub struct SubExtLoader {
    registry: ProviderRegistry,

    /// `sub_ext -> plan.json path`. Populated from CLI/env/config; consulted
    /// on the first LOAD of the sub-ext to compose its provider bytes.
    pub sub_ext_plan_paths: BTreeMap<String, PathBuf>,

    /// `sub_ext -> bridge.wasm path`. Populated from CLI/env/config; consulted
    /// on the first LOAD of the sub-ext to pick the wasm the guest actually
    /// instantiates. Split from the flat `<extensions-dir>/<name>.wasm` path
    /// so a sub-ext's bridge can live anywhere on disk.
    pub sub_ext_bridge_paths: BTreeMap<String, PathBuf>,

    /// `plan-component-id -> upstream-sub-ext-id`. Every plan component
    /// whose digest is the 32-byte all-zeros placeholder is chain-resolved
    /// through this map before the current plan composes: the referenced
    /// upstream is composed first (recursing back through
    /// [`SubExtLoader::materialize_sub_ext_provider`] so the same idempotence
    /// guard dedups a shared upstream) and its composed digest is spliced
    /// into an in-memory clone of the current plan.
    pub sub_ext_derived_from: BTreeMap<String, String>,

    /// `sub_ext -> prebuilt-composed-wasm-path`. Short-circuits the
    /// plan-driven compose: if an entry exists for `sub_ext`, the loader
    /// registers the prebuilt wasm directly under `sub_ext_provider_id`
    /// and skips reading/parsing/composing the plan JSON entirely. Used
    /// when the plan-driven per-sub-ext composed artifact would leak
    /// unresolved sibling-wasm imports (the `postgis-core-composed`
    /// 19-import gap) and the application ships a self-contained
    /// monolithic composed wasm as a fallback. Takes precedence over
    /// `sub_ext_plan_paths` for the same key.
    pub sub_ext_prebuilt_paths: BTreeMap<String, PathBuf>,

    /// Blob CAS root (default `<cwd>/.compose/blobs`). Every plan
    /// component's digest must resolve to a blob under this root — the
    /// application layer (postgis-wasm's `plans/verify.sh` or an
    /// equivalent) is responsible for pre-seeding the CAS.
    pub blob_cas_path: PathBuf,

    /// compose-core cache directory (default `<cwd>/.compose/cache`). Keyed
    /// by `emit_key = H(plan bytes)`; a repeat compose of the same plan
    /// hits this and produces zero disk I/O beyond the read.
    pub compose_cache_path: PathBuf,

    /// Idempotence guard — sub-exts already materialized this session.
    materialized_sub_exts: HashSet<String>,

    /// `sub_ext -> composed CompositionResult.digest`. Populated by
    /// [`SubExtLoader::materialize_sub_ext_provider`] BEFORE the idempotence
    /// insert so a downstream sub-ext that hits the early-return path still
    /// sees the digest cached here.
    composed_digests: HashMap<String, Vec<u8>>,
}

impl SubExtLoader {
    /// Blank loader with default CAS/cache paths (relative to cwd) and no
    /// entries. Populate the public map fields (or use
    /// [`SubExtLoader::from_env`]) before calling
    /// [`SubExtLoader::materialize_sub_ext_provider`].
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry,
            sub_ext_plan_paths: BTreeMap::new(),
            sub_ext_bridge_paths: BTreeMap::new(),
            sub_ext_derived_from: BTreeMap::new(),
            sub_ext_prebuilt_paths: BTreeMap::new(),
            blob_cas_path: PathBuf::from(".compose/blobs"),
            compose_cache_path: PathBuf::from(".compose/cache"),
            materialized_sub_exts: HashSet::new(),
            composed_digests: HashMap::new(),
        }
    }

    /// Loader seeded from the three env vars (`DUCKLINK_SUB_EXT_PLANS`,
    /// `DUCKLINK_SUB_EXT_BRIDGES`, `DUCKLINK_SUB_EXT_DERIVED`) plus the
    /// optional `DUCKLINK_COMPOSE_BLOB_CAS` / `DUCKLINK_COMPOSE_CACHE`
    /// overrides for the CAS/cache dirs. Malformed entries are logged and
    /// skipped so a typo doesn't abort startup.
    pub fn from_env(registry: ProviderRegistry) -> Self {
        let mut this = Self::new(registry);
        this.sub_ext_plan_paths = parse_map_env("DUCKLINK_SUB_EXT_PLANS");
        this.sub_ext_bridge_paths = parse_map_env("DUCKLINK_SUB_EXT_BRIDGES");
        this.sub_ext_derived_from = parse_map_env("DUCKLINK_SUB_EXT_DERIVED")
            .into_iter()
            .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
            .collect();
        this.sub_ext_prebuilt_paths = parse_map_env("DUCKLINK_SUB_EXT_PREBUILT");
        if let Ok(p) = std::env::var("DUCKLINK_COMPOSE_BLOB_CAS") {
            if !p.trim().is_empty() {
                this.blob_cas_path = PathBuf::from(p.trim());
            }
        }
        if let Ok(p) = std::env::var("DUCKLINK_COMPOSE_CACHE") {
            if !p.trim().is_empty() {
                this.compose_cache_path = PathBuf::from(p.trim());
            }
        }
        this
    }

    /// True if `sub_ext` has an associated bridge wasm path. `LOAD`'s
    /// sub-ext branch takes the bridge from this map instead of resolving
    /// through the flat `<extensions-dir>/<name>.wasm` shortcut.
    pub fn has_bridge(&self, sub_ext: &str) -> bool {
        self.sub_ext_bridge_paths.contains_key(sub_ext)
    }

    /// Bridge wasm path for `sub_ext`, if configured.
    pub fn bridge_path(&self, sub_ext: &str) -> Option<&Path> {
        self.sub_ext_bridge_paths.get(sub_ext).map(PathBuf::as_path)
    }

    /// Whether this sub-ext has already been composed + registered.
    /// Exposed so a test (or the LOAD path's diagnostics) can observe the
    /// idempotence guard without racing on repeated composes.
    pub fn is_materialized(&self, sub_ext: &str) -> bool {
        self.materialized_sub_exts.contains(sub_ext)
    }

    /// Compose the sub-ext's plan, write the composed bytes to a stable
    /// file under `<blob_cas>/composed-providers/`, and register the file
    /// under `sub_ext_provider_id(sub_ext)` in the shared
    /// `ProviderRegistry`. Ported from
    /// `datafission::df-plugin-loader::UnifiedPluginLoader::materialize_sub_ext_provider`
    /// (`~/git/datafission/crates/df-plugin-loader/src/loader.rs:337`); the
    /// only changes are:
    ///   * `&mut self` instead of `&self` + `Mutex`-guarded fields
    ///     (ducklink already threads `Arc<Mutex<ExtensionManager>>`, so an
    ///     inner mutex would double-lock);
    ///   * registration goes through this crate's `ProviderRegistry` handle
    ///     (which owns the wasmtime engine) rather than
    ///     `wasm_extension::register_provider_path`.
    pub fn materialize_sub_ext_provider(&mut self, sub_ext: &str) -> Result<String, SubExtError> {
        let provider_id = sub_ext_provider_id(sub_ext);

        if self.materialized_sub_exts.contains(sub_ext) {
            return Ok(provider_id);
        }

        // Phase D / Option B short-circuit: if the sub-ext has a
        // prebuilt, self-contained composed wasm on disk, register that
        // directly and skip the plan-driven compose entirely. This is
        // the escape hatch for when the plan-driven per-sub-ext composed
        // artifact would leak unresolved sibling-wasm imports (e.g. the
        // `postgis-core-composed` 19-import gap) and the application
        // ships a monolithic composed wasm as a self-contained fallback.
        //
        // Preemption order:
        //   1. materialized guard (above) — no-op on repeat LOAD
        //   2. prebuilt shortcut (here) — register file, done
        //   3. plan-driven compose (below) — the normal path
        if let Some(prebuilt) = self.sub_ext_prebuilt_paths.get(sub_ext).cloned() {
            if !prebuilt.exists() {
                return Err(SubExtError::mat(
                    sub_ext,
                    &prebuilt,
                    format!("prebuilt provider wasm not found at {}", prebuilt.display()),
                ));
            }
            self.registry
                .register_provider(provider_id.clone(), &prebuilt)
                .map_err(|reason| SubExtError::ProviderRegistration {
                    id: provider_id.clone(),
                    path: prebuilt.clone(),
                    reason,
                })?;
            eprintln!(
                "[sub-ext] '{sub_ext}' short-circuit: registered prebuilt monolith \
                 {} as provider '{provider_id}'; skipping plan-driven compose",
                prebuilt.display()
            );
            let digest = {
                use sha2::{Digest, Sha256};
                let bytes = std::fs::read(&prebuilt).map_err(|e| {
                    SubExtError::mat(sub_ext, &prebuilt, format!("digest read: {e}"))
                })?;
                Sha256::digest(&bytes).to_vec()
            };
            self.composed_digests.insert(sub_ext.to_string(), digest);
            self.materialized_sub_exts.insert(sub_ext.to_string());
            return Ok(provider_id);
        }

        let plan_path = self
            .sub_ext_plan_paths
            .get(sub_ext)
            .cloned()
            .ok_or_else(|| SubExtError::PlanMissing {
                sub_ext: sub_ext.to_string(),
            })?;

        let plan_bytes = std::fs::read(&plan_path)
            .map_err(|e| SubExtError::mat(sub_ext, &plan_path, format!("read plan: {e}")))?;

        let mut plan: PlanV1 = serde_json::from_slice(&plan_bytes).map_err(|e| {
            SubExtError::mat(sub_ext, &plan_path, format!("parse plan as JSON: {e}"))
        })?;

        // Chain-resolve every zero-digest component through the derived-from
        // map (recursing back into this method so the idempotence guard
        // dedups shared upstreams). This must run BEFORE the EmitHandler is
        // constructed — the recursive call opens its own BlobStore/EmitHandler,
        // safe because their roots come from `self` and don't nest.
        for component in plan.components.iter_mut() {
            if !is_zero_digest(&component.digest) {
                continue;
            }
            let upstream = self
                .sub_ext_derived_from
                .get(&component.id)
                .cloned()
                .ok_or_else(|| {
                    SubExtError::mat(
                        sub_ext,
                        &plan_path,
                        format!(
                            "component '{}' has an all-zero digest placeholder but no \
                             sub_ext_derived_from entry maps it to an upstream sub-ext",
                            component.id
                        ),
                    )
                })?;

            self.materialize_sub_ext_provider(&upstream)?;

            let upstream_digest =
                self.composed_digests
                    .get(&upstream)
                    .cloned()
                    .ok_or_else(|| {
                        SubExtError::mat(
                            sub_ext,
                            &plan_path,
                            format!(
                                "upstream sub-ext '{}' for derived component '{}' composed \
                                 but produced no cached digest",
                                upstream, component.id
                            ),
                        )
                    })?;
            component.digest = upstream_digest;
        }

        // Compose. compose-core keys on `emit_key = H(plan bytes)`; a repeat
        // compose with the same plan cache-hits.
        let blobs = BlobStore::new(self.blob_cas_path.clone(), COMPOSE_MAX_BLOB_SIZE).map_err(
            |e| {
                SubExtError::mat(
                    sub_ext,
                    &plan_path,
                    format!("open blob CAS at {}: {e}", self.blob_cas_path.display()),
                )
            },
        )?;
        let clock = SystemClock::shared();
        let events = EventCollector::new(clock);
        std::fs::create_dir_all(&self.compose_cache_path).map_err(|e| {
            SubExtError::mat(
                sub_ext,
                &plan_path,
                format!(
                    "create compose cache at {}: {e}",
                    self.compose_cache_path.display()
                ),
            )
        })?;

        let emit = EmitHandler::new(blobs.clone(), events, self.compose_cache_path.clone());
        let result = emit
            .compose(&plan)
            .map_err(|e| SubExtError::mat(sub_ext, &plan_path, format!("compose: {e}")))?;

        // Stash the composed digest BEFORE the idempotence insert so a
        // downstream sub-ext that recurses in and hits the early return still
        // sees the digest cached here.
        self.composed_digests
            .insert(sub_ext.to_string(), result.digest.clone());

        // Materialize the composed bytes to a stable file so the process-
        // global `ProviderRegistry` can register it by path.
        let composed_bytes = blobs.get(&result.digest).map_err(|e| {
            SubExtError::mat(
                sub_ext,
                &plan_path,
                format!("read composed artifact from CAS: {e}"),
            )
        })?;
        let provider_dir = self.blob_cas_path.join("composed-providers");
        std::fs::create_dir_all(&provider_dir).map_err(|e| {
            SubExtError::mat(
                sub_ext,
                &plan_path,
                format!("create {}: {e}", provider_dir.display()),
            )
        })?;
        let provider_path = provider_dir.join(format!("{provider_id}.wasm"));
        std::fs::write(&provider_path, &composed_bytes).map_err(|e| {
            SubExtError::mat(
                sub_ext,
                &plan_path,
                format!("write {}: {e}", provider_path.display()),
            )
        })?;

        self.registry
            .register_provider(provider_id.clone(), &provider_path)
            .map_err(|reason| SubExtError::ProviderRegistration {
                id: provider_id.clone(),
                path: provider_path.clone(),
                reason,
            })?;

        self.materialized_sub_exts.insert(sub_ext.to_string());
        Ok(provider_id)
    }
}

/// Parse `<name>=<path>:<name>=<path>...` into a BTreeMap. `:` is the
/// entry separator (matches datafission's convention) but a `,` is
/// also accepted to be forgiving. Malformed entries are logged to stderr
/// and skipped.
fn parse_map_env(var: &str) -> BTreeMap<String, PathBuf> {
    let spec = match std::env::var(var) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return BTreeMap::new(),
    };
    let mut out = BTreeMap::new();
    for entry in spec
        .split(|c: char| c == ':' || c == ',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        match entry.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => {
                out.insert(k.trim().to_string(), PathBuf::from(v.trim()));
            }
            _ => eprintln!(
                "[sub-ext] {var}: skipping malformed entry '{entry}' (expected name=path)"
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_is_sub_ext_dash_composed() {
        assert_eq!(sub_ext_provider_id("postgis_core"), "postgis_core-composed");
        assert_eq!(sub_ext_provider_id("mobilitydb"), "mobilitydb-composed");
    }

    #[test]
    fn is_zero_digest_rejects_empty_and_nonzero() {
        assert!(!is_zero_digest(&[]));
        assert!(is_zero_digest(&[0u8; 32]));
        assert!(!is_zero_digest(&[0u8, 0, 1, 0]));
    }

    #[test]
    fn prebuilt_shortcut_registers_wasm_and_skips_compose() {
        use wasmtime::{Config, Engine};

        // A minimal valid WebAssembly component (magic + component version).
        // Enough for `Component::from_binary` to succeed at registration
        // time; the resolve-by-id path never fires in this unit test so
        // the exports don't matter.
        let component_bytes: Vec<u8> = vec![
            0x00, 0x61, 0x73, 0x6d, // \0asm
            0x0d, 0x00, 0x01, 0x00, // component version
        ];
        let tmp = tempfile::tempdir().unwrap();
        let prebuilt = tmp.path().join("postgis-composed.wasm");
        std::fs::write(&prebuilt, &component_bytes).unwrap();

        let mut cfg = Config::new();
        cfg.wasm_component_model(true);
        let engine = Engine::new(&cfg).unwrap();
        let registry = ProviderRegistry::new(engine);
        let mut loader = SubExtLoader::new(registry.clone());

        // Register prebuilt but no plan; materialize must register-then-return.
        loader
            .sub_ext_prebuilt_paths
            .insert("postgis_core".to_string(), prebuilt.clone());

        let id = loader
            .materialize_sub_ext_provider("postgis_core")
            .expect("prebuilt shortcut must succeed with no plan");
        assert_eq!(id, "postgis_core-composed");
        assert!(loader.is_materialized("postgis_core"));

        // Second call is a no-op via the materialized guard.
        assert_eq!(
            loader.materialize_sub_ext_provider("postgis_core").unwrap(),
            id
        );

        // Provider is registered (compile-time only, no resident instance).
        assert_eq!(registry.resident_count(&id), 0);
    }

    #[test]
    fn parse_map_env_handles_colon_and_comma_separators() {
        // Use unique env var names so parallel test threads don't clobber
        // each other. std::env::set_var is `unsafe` from Rust 2024 onward;
        // the crate is edition 2021 so the plain call is stable here.
        std::env::set_var("DUCKLINK_TEST_MAP_ENV_A", "a=/x:b=/y");
        let m = parse_map_env("DUCKLINK_TEST_MAP_ENV_A");
        assert_eq!(m.get("a"), Some(&PathBuf::from("/x")));
        assert_eq!(m.get("b"), Some(&PathBuf::from("/y")));

        std::env::set_var("DUCKLINK_TEST_MAP_ENV_B", "c=/p,d=/q");
        let m = parse_map_env("DUCKLINK_TEST_MAP_ENV_B");
        assert_eq!(m.get("c"), Some(&PathBuf::from("/p")));
        assert_eq!(m.get("d"), Some(&PathBuf::from("/q")));

        std::env::remove_var("DUCKLINK_TEST_MAP_ENV_C");
        assert!(parse_map_env("DUCKLINK_TEST_MAP_ENV_C").is_empty());

        std::env::remove_var("DUCKLINK_TEST_MAP_ENV_A");
        std::env::remove_var("DUCKLINK_TEST_MAP_ENV_B");
    }
}
