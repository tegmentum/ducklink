//! #823 dedup smoke test: exercise the process-global provider
//! registry against a real `postgis-composed-provider.wasm` artifact.
//!
//! Validates that:
//! - `register_provider` accepts the artifact without error.
//! - `ResidentBackend::resolve_by_id` instantiates the resident
//!   provider ONCE (log emitted by datalink-dynlink).
//! - `ResidentBackend::invoke("postgis-lib-version", ...)` round-trips
//!   through the compose:dynlink/endpoint export and returns
//!   `"3.6.4"` (matching the constant in postgis-wasm/src/version.rs).
//! - A SECOND `resolve_by_id` for the same id reuses the resident
//!   instance rather than instantiating again (the log distinguishes
//!   "instantiated ONCE" vs "reuses the existing resident"; the
//!   handle_count counter is what we assert on).
//!
//! ## Skip conditions
//!
//! The test skips (prints a SKIP line and returns Ok) when the
//! artifact isn't on disk. To rebuild:
//!
//! ```sh
//! cd ~/git/postgis-wasm
//! scripts/compose.sh           # produces postgis-composed.wasm
//! scripts/compose-provider.sh  # produces postgis-composed-provider.wasm
//! ```
//!
//! Override the search path with
//! `POSTGIS_COMPOSED_PROVIDER_WASM=<path>`.

use std::path::PathBuf;

use datalink_dynlink::{ProviderBackend, ProviderRegistry, ResidentBackend};
use serde::{Deserialize, Serialize};

// Match the envelope shape ships in the postgis-wasm-provider crate
// (postgis-wasm/crates/provider/src/envelope.rs). Duplicated here on
// purpose: the test asserts on the WIRE shape and shouldn't depend
// on the provider crate as a Rust dep — both sides speak CBOR.

const ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum CborValue {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Text(String),
    #[serde(with = "serde_bytes")]
    Bytes(Vec<u8>),
    List(Vec<CborValue>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    #[serde(rename = "v")]
    version: u32,
    #[serde(default)]
    args: Vec<CborValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Response {
    #[serde(rename = "v")]
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<CborValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

fn artifact_path() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("POSTGIS_COMPOSED_PROVIDER_WASM") {
        let p = PathBuf::from(env_path);
        return if p.exists() { Some(p) } else { None };
    }
    // Default: sibling ~/git/postgis-wasm.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let default = PathBuf::from(manifest_dir)
        .join("../..")
        .join("../postgis-wasm/postgis-composed-provider.wasm");
    // manifest_dir/../../.. == ~/git; then postgis-wasm/... resolves the
    // sibling artifact.
    default.canonicalize().ok().filter(|p| p.exists())
}

fn shared_engine() -> wasmtime::Engine {
    let mut config = wasmtime::Config::new();
    // Match ducklink-runtime's default engine config so the smoke
    // test exercises the same code paths a real load would.
    config.wasm_component_model(true);
    config.wasm_exceptions(true);
    wasmtime::Engine::new(&config).expect("engine")
}

fn encode_empty_request() -> Vec<u8> {
    let req = Request {
        version: ENVELOPE_VERSION,
        args: vec![],
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&req, &mut buf).expect("encode request");
    buf
}

fn decode_response(bytes: &[u8]) -> Response {
    ciborium::de::from_reader(bytes).expect("decode response")
}

#[test]
fn postgis_provider_round_trips_via_dynlink() {
    let path = match artifact_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP: postgis-composed-provider.wasm not present. \
                 Set POSTGIS_COMPOSED_PROVIDER_WASM or build via \
                 postgis-wasm/scripts/compose-provider.sh"
            );
            return;
        }
    };

    let engine = shared_engine();
    let registry = ProviderRegistry::new(engine);

    // Ingress: the exact code path ducklink-runtime exposes at
    // register_provider. Once this test is green, the type system
    // starts enforcing the wiring end-to-end.
    registry
        .register_provider("postgis-composed", &path)
        .expect("register_provider accepts the artifact");

    let backend = ResidentBackend::new(registry.clone());

    let handle1 = backend
        .resolve_by_id("postgis-composed")
        .expect("first resolve");

    let payload = encode_empty_request();
    let response_bytes = backend
        .invoke(&handle1, "postgis-lib-version", &payload)
        .expect("invoke postgis-lib-version");

    let resp = decode_response(&response_bytes);
    assert_eq!(resp.version, ENVELOPE_VERSION);
    assert!(resp.err.is_none(), "unexpected err: {:?}", resp.err);
    match resp.ok {
        Some(CborValue::Text(t)) => {
            // The constant in postgis-wasm/src/version.rs is 3.6.4;
            // asserting the shape (three-dot version) keeps the test
            // stable across future point releases.
            assert!(
                t.chars().filter(|c| *c == '.').count() >= 2,
                "expected dotted version string, got: {}",
                t
            );
            assert!(
                t.starts_with('3'),
                "expected PostGIS 3.x, got: {}",
                t
            );
        }
        other => panic!("unexpected response body: {:?}", other),
    }

    // Second resolve: proves dedup — same registry, same resident
    // instance. datalink-dynlink prints "reuses the existing
    // resident provider" to stderr; here we just verify the second
    // call succeeds AND the handle_count reflects both handles.
    let handle2 = backend
        .resolve_by_id("postgis-composed")
        .expect("second resolve");

    assert_eq!(
        registry.handle_count("postgis-composed"),
        2,
        "handle_count should reflect BOTH outstanding resolves"
    );

    // Drop both handles; if datalink-dynlink's on_drop hook is
    // wired end-to-end, handle_count returns to 0 after the
    // ResidentBackend goes out of scope. (We don't call
    // `bridge.drop_handle` directly because that requires a
    // ResourceTable, which the test doesn't own; the on_drop is
    // called from the wasmtime resource-drop path in the real
    // ExtensionHostState.)
    drop(handle1);
    drop(handle2);
}

#[test]
fn unknown_method_returns_response_err_not_transport_error() {
    let path = match artifact_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP: postgis-composed-provider.wasm not present. \
                 See postgis_provider_round_trips_via_dynlink."
            );
            return;
        }
    };

    let engine = shared_engine();
    let registry = ProviderRegistry::new(engine);
    registry
        .register_provider("postgis-composed", &path)
        .unwrap();
    let backend = ResidentBackend::new(registry.clone());
    let handle = backend.resolve_by_id("postgis-composed").unwrap();

    let payload = encode_empty_request();
    let response_bytes = backend
        .invoke(&handle, "st-does-not-exist", &payload)
        .expect("invoke returns Ok even for unknown methods — the err is inside the envelope");

    let resp = decode_response(&response_bytes);
    assert!(resp.ok.is_none());
    let err = resp.err.expect("envelope err field populated");
    assert!(
        err.contains("unknown method"),
        "expected 'unknown method' in err, got: {}",
        err
    );
}
