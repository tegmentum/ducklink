//! Phase D: materialize_sub_ext_provider integration test.
//!
//! Configures `sub_ext_bridge_paths["postgis_core"] = <fixture bridge wasm>`
//! and `sub_ext_plan_paths["postgis_core"] = <fixture plan.json>` against
//! an in-tempdir compose CAS, seeds the CAS with a minimal component
//! blob, then calls `SubExtLoader::materialize_sub_ext_provider("postgis_core")`
//! and asserts:
//!
//!   * the returned provider id is `postgis_core-composed`,
//!   * the composed bytes are materialized to
//!     `<cas>/composed-providers/postgis_core-composed.wasm`,
//!   * the composed provider is now registered in the shared
//!     `ProviderRegistry` (the same registry `LOAD` consults for a bridge
//!     that imports `compose:dynlink/linker.resolve-by-id("postgis_core-composed")`),
//!   * a second call is a no-op (idempotence guard), and
//!   * `has_bridge(...)` correctly reports which sub-exts should take the
//!     bridge branch in `ensure_extension_loaded`.
//!
//! Mirrors the shape of `tests/compose_dynlink_dlopen.rs` (same registry
//! + same engine wiring) so it fails alongside that test when the
//! `compose:dynlink` plumbing regresses. The bridge wasm is a
//! placeholder (a minimal component so `.exists()` succeeds if the
//! caller ever runs the ensure-load side, and no more) — Phase D's
//! contract is registry population, not bridge instantiation, which
//! `compose_dynlink_dlopen.rs` already covers end-to-end.

use std::path::PathBuf;

use compose_core::blobs::BlobStore;
use compose_core::types::{ComponentSpec, PlanV1, Policy};
use ducklink_host::{sub_ext_provider_id, ProviderRegistry, SubExtLoader};
use wasmtime::{Config, Engine};

/// Minimal valid WebAssembly component (magic + component version).
/// Same fixture the compose-core tests use for plan composition.
fn minimal_component_bytes() -> Vec<u8> {
    vec![
        0x00, 0x61, 0x73, 0x6d, // \0asm
        0x0d, 0x00, 0x01, 0x00, // component version
    ]
}

fn test_engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config).expect("engine")
}

#[test]
fn materialize_sub_ext_provider_composes_registers_and_is_idempotent() {
    let workdir = tempfile::tempdir().expect("tempdir");
    let cas = workdir.path().join("blobs");
    let cache = workdir.path().join("cache");
    std::fs::create_dir_all(&cas).unwrap();
    std::fs::create_dir_all(&cache).unwrap();

    // 1. Seed the blob CAS with a valid minimal component and get its
    //    content digest. compose-core's EmitHandler will look up the plan's
    //    root component by this digest and emit it (a single-component plan
    //    is a passthrough — the emit result's digest equals the input's).
    let blobs = BlobStore::new(cas.clone(), 1 << 20).expect("blob store");
    let component_bytes = minimal_component_bytes();
    let digest = blobs.put(&component_bytes).expect("put component blob");

    // 2. Write a PlanV1 JSON on disk that points at that blob as its
    //    single root component. materialize_sub_ext_provider parses this
    //    from disk before composing.
    let plan = PlanV1 {
        version: "1".to_string(),
        root: "root".to_string(),
        components: vec![ComponentSpec {
            id: "root".to_string(),
            digest: digest.clone(),
            source: None,
        }],
        bindings: vec![],
        secrets: vec![],
        policy: Policy::default(),
        linkage: Default::default(),
        explicit_exports: vec![],
    };
    let plan_path = workdir.path().join("postgis-composed.plan.json");
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();

    // 3. Also drop a placeholder bridge wasm so `has_bridge` -> path -> exists
    //    exercises the same code path the extension load will take. The
    //    bytes don't need to be a valid component here — this test only
    //    covers the registry-population side of Phase D (the actual
    //    bridge instantiation is covered by `compose_dynlink_dlopen.rs`).
    let bridge_path = workdir.path().join("postgis_core_bridge.wasm");
    std::fs::write(&bridge_path, minimal_component_bytes()).unwrap();

    // 4. Build the shared engine + provider registry (the same wiring
    //    ExtensionManager uses at runtime) and seed the SubExtLoader.
    let engine = test_engine();
    let registry = ProviderRegistry::new(engine.clone());
    let mut loader = SubExtLoader::new(registry.clone());
    loader.blob_cas_path = cas.clone();
    loader.compose_cache_path = cache.clone();
    loader
        .sub_ext_plan_paths
        .insert("postgis_core".to_string(), plan_path.clone());
    loader
        .sub_ext_bridge_paths
        .insert("postgis_core".to_string(), bridge_path.clone());

    // 5. has_bridge routes LOAD through the sub-ext branch; an unknown
    //    name falls through to the standard extensions-dir resolution.
    assert!(loader.has_bridge("postgis_core"));
    assert!(!loader.has_bridge("httpfs"));
    assert_eq!(
        loader.bridge_path("postgis_core"),
        Some(bridge_path.as_path())
    );

    // 6. Materialize. Returns the canonical provider id and, as a
    //    side effect, (a) writes composed bytes to the CAS's
    //    `composed-providers/` subdir, (b) registers those bytes under
    //    that id in the shared registry.
    let provider_id = loader
        .materialize_sub_ext_provider("postgis_core")
        .expect("materialize sub-ext provider");
    assert_eq!(provider_id, "postgis_core-composed");
    assert_eq!(provider_id, sub_ext_provider_id("postgis_core"));

    // 7. Composed wasm is written to a stable, path-registerable file.
    let composed_wasm = cas
        .join("composed-providers")
        .join("postgis_core-composed.wasm");
    assert!(
        composed_wasm.exists(),
        "composed provider bytes must be materialized at {}",
        composed_wasm.display()
    );

    // 8. Idempotence — a re-issued LOAD hits the materialized guard and
    //    no-ops. It must NOT touch the CAS / registry a second time.
    assert!(loader.is_materialized("postgis_core"));
    let provider_id_again = loader
        .materialize_sub_ext_provider("postgis_core")
        .expect("idempotent materialize");
    assert_eq!(provider_id_again, provider_id);

    // 9. Provider is now resolvable by any bridge that plugs it.
    //    resident_count starts at 0 (materialization only *registers* +
    //    compiles the provider; instantiation is lazy on first
    //    resolve-by-id call from a bridge). Non-zero would indicate an
    //    unwanted eager instantiation.
    assert_eq!(
        registry.resident_count(&provider_id),
        0,
        "provider must be registered but NOT yet instantiated after materialize"
    );
}

#[test]
fn materialize_sub_ext_provider_reports_missing_plan() {
    let engine = test_engine();
    let registry = ProviderRegistry::new(engine.clone());
    let mut loader = SubExtLoader::new(registry);
    // No plan registered → PlanMissing error, no partial materialization.
    let err = loader
        .materialize_sub_ext_provider("unknown_ext")
        .expect_err("must report PlanMissing when nothing is configured");
    match err {
        ducklink_host::SubExtError::PlanMissing { sub_ext } => {
            assert_eq!(sub_ext, "unknown_ext");
        }
        other => panic!("expected PlanMissing, got {other:?}"),
    }
    assert!(!loader.is_materialized("unknown_ext"));
}

#[test]
fn has_bridge_and_bridge_path_wire_the_load_time_branch() {
    let engine = test_engine();
    let registry = ProviderRegistry::new(engine);
    let mut loader = SubExtLoader::new(registry);
    let bridge_path = PathBuf::from("/tmp/postgis_core_bridge.wasm");
    loader
        .sub_ext_bridge_paths
        .insert("postgis_core".to_string(), bridge_path.clone());
    assert!(loader.has_bridge("postgis_core"));
    assert_eq!(
        loader.bridge_path("postgis_core"),
        Some(bridge_path.as_path())
    );
    assert!(!loader.has_bridge("mobilitydb"));
    assert_eq!(loader.bridge_path("mobilitydb"), None);
}

#[test]
fn prebuilt_only_config_routes_through_sub_ext_branch() {
    // In the pure-monolith mode the user sets DUCKLINK_SUB_EXT_PREBUILT
    // alone (per the postgis-wasm README). `has_bridge` must still return
    // true for that sub-ext so LOAD takes the sub-ext branch instead of
    // the flat resolver, and `bridge_path` must fall back to the prebuilt
    // wasm so the .expect() in ExtensionManager doesn't panic.
    let engine = test_engine();
    let registry = ProviderRegistry::new(engine);
    let mut loader = SubExtLoader::new(registry);
    let prebuilt = PathBuf::from("/tmp/postgis-monolith-provider.wasm");
    loader
        .sub_ext_prebuilt_paths
        .insert("postgis_core".to_string(), prebuilt.clone());
    assert!(loader.has_bridge("postgis_core"));
    assert_eq!(loader.bridge_path("postgis_core"), Some(prebuilt.as_path()));
    // Explicit bridge still wins when both are set.
    let bridge = PathBuf::from("/tmp/postgis_core_bridge.wasm");
    loader
        .sub_ext_bridge_paths
        .insert("postgis_core".to_string(), bridge.clone());
    assert_eq!(loader.bridge_path("postgis_core"), Some(bridge.as_path()));
}

#[test]
fn mixed_mode_prebuilt_upstream_seeds_cas_for_derived_plan() {
    // The regression this pins: postgis_core registered as prebuilt +
    // postgis_raster driven by a plan that has a zero-digest placeholder
    // component derived from postgis_core. Before the fix the prebuilt
    // shortcut cached sha256(prebuilt) in composed_digests but never
    // wrote the bytes into the CAS, so the raster compose walked the
    // derived-from chain, spliced the prebuilt digest into the raster
    // plan, then failed with BlobNotFound when compose-core tried to
    // read those bytes back from the CAS.
    let workdir = tempfile::tempdir().expect("tempdir");
    let cas = workdir.path().join("blobs");
    let cache = workdir.path().join("cache");
    std::fs::create_dir_all(&cas).unwrap();
    std::fs::create_dir_all(&cache).unwrap();

    // Prebuilt monolith for postgis_core. Contents are the minimal valid
    // component blob so wasmtime accepts them at registration time.
    let prebuilt = workdir.path().join("postgis-monolith-provider.wasm");
    std::fs::write(&prebuilt, minimal_component_bytes()).unwrap();

    // Plan for postgis_raster: one component with an all-zero digest
    // that will be spliced via sub_ext_derived_from -> postgis_core.
    // A single-component plan composes as a passthrough — the result
    // digest equals the spliced input digest — so we don't need to
    // model raster-specific bytes here.
    let raster_plan = PlanV1 {
        version: "1".to_string(),
        root: "root".to_string(),
        components: vec![ComponentSpec {
            id: "root".to_string(),
            digest: vec![0u8; 32],
            source: None,
        }],
        bindings: vec![],
        secrets: vec![],
        policy: Policy::default(),
        linkage: Default::default(),
        explicit_exports: vec![],
    };
    let raster_plan_path = workdir.path().join("postgis-raster.plan.json");
    std::fs::write(
        &raster_plan_path,
        serde_json::to_vec_pretty(&raster_plan).unwrap(),
    )
    .unwrap();

    let engine = test_engine();
    let registry = ProviderRegistry::new(engine);
    let mut loader = SubExtLoader::new(registry);
    loader.blob_cas_path = cas.clone();
    loader.compose_cache_path = cache;
    loader
        .sub_ext_prebuilt_paths
        .insert("postgis_core".to_string(), prebuilt.clone());
    loader
        .sub_ext_plan_paths
        .insert("postgis_raster".to_string(), raster_plan_path);
    loader
        .sub_ext_derived_from
        .insert("root".to_string(), "postgis_core".to_string());

    // Materialize the downstream. This recurses into postgis_core (which
    // hits the prebuilt shortcut), splices the resolved digest into the
    // raster plan, then composes. Without the CAS-write fix, the compose
    // step returns BlobNotFound for the spliced digest.
    let raster_id = loader
        .materialize_sub_ext_provider("postgis_raster")
        .expect("mixed-mode chain must resolve prebuilt upstream via CAS");
    assert_eq!(raster_id, "postgis_raster-composed");
    assert!(loader.is_materialized("postgis_core"));
    assert!(loader.is_materialized("postgis_raster"));

    // The prebuilt's bytes are readable from the compose CAS at the
    // digest cached under postgis_core — this is the invariant every
    // downstream compose relies on.
    let blobs = BlobStore::new(cas, 1 << 20).expect("open CAS");
    let prebuilt_bytes = std::fs::read(&prebuilt).unwrap();
    let digest = compose_core::blobs::compute_digest(&prebuilt_bytes);
    let round_trip = blobs
        .get(&digest)
        .expect("prebuilt bytes must be present in CAS after shortcut");
    assert_eq!(round_trip, prebuilt_bytes);
}
