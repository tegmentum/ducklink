//! Native (wasmtime) analog of the browser dynlink proof.
//!
//! Registers the framework's `dynlink_echo_provider.wasm` under the id
//! `"provider"`, then instantiates the framework's `dynlink-dlopen-guest`
//! (`wasi:cli/run`) component through ducklink-host's wasmtime + a linker
//! carrying the `compose:dynlink/linker` host import. Driving the guest's
//! `run` makes it `resolve_by_id("provider").invoke("upper", "hello from
//! dlopen")` and print the uppercased result. We capture stdout and
//! assert it contains `HELLO FROM DLOPEN`.
//!
//! Also asserts the SHARED-copy property: resolving the provider twice
//! (the guest resolves once; the test resolves a second time directly)
//! reuses ONE resident provider instance.

use std::path::PathBuf;

use ducklink_host::ProviderRegistry;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::bindings::sync::Command;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{ResourceTable, WasiCtxBuilder};

// Re-exported from the crate for the test (the test path mirrors the
// real load-path wiring: imports_linker gate + add_to_linker).
use ducklink_host::compose_dynlink_test_support as cds;

fn orchestration_repo() -> PathBuf {
    // The prebuilt example components live in the sibling
    // webassembly-component-orchestration repo.
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join("git/webassembly-component-orchestration")
}

fn guest_wasm() -> PathBuf {
    orchestration_repo().join(
        "examples/dynlink-dlopen-guest/target/wasm32-wasip2/release/dynlink-dlopen-guest.wasm",
    )
}

fn provider_wasm() -> PathBuf {
    orchestration_repo().join(
        "examples/dynlink-echo-provider/target/wasm32-wasip2/release/dynlink_echo_provider.wasm",
    )
}

fn test_engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config).expect("engine")
}

#[test]
fn dlopen_guest_invokes_shared_provider_and_prints_uppercase() {
    let guest_path = guest_wasm();
    let provider_path = provider_wasm();
    if !guest_path.exists() || !provider_path.exists() {
        eprintln!(
            "skipping: prebuilt example components not found ({}, {})",
            guest_path.display(),
            provider_path.display()
        );
        return;
    }

    let engine = test_engine();

    // 1. Build the shared provider registry and register the echo provider
    //    under id "provider". register_provider compiles it now; the
    //    resident instance is materialized lazily on first resolve.
    let registry = ProviderRegistry::new(engine.clone());
    registry
        .register_provider("provider", &provider_path)
        .expect("register echo provider");
    assert_eq!(
        registry.resident_count("provider"),
        0,
        "provider must not be instantiated until first resolve"
    );

    // 2. Build the guest linker over ducklink's DynState: WASI + the
    //    conditional compose:dynlink/linker host import (only added because
    //    the guest imports it — the imports_linker gate mirrors the real
    //    load path).
    let guest_component = Component::from_file(&engine, &guest_path).expect("load guest");
    assert!(
        cds::imports_linker(&engine, &guest_component),
        "the dlopen guest must import compose:dynlink/linker"
    );

    let mut linker: Linker<cds::DynState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("wasi linker");
    cds::add_to_linker(&mut linker).expect("compose:dynlink/linker host import");

    // Capture stdout so we can assert the printed result.
    let stdout = MemoryOutputPipe::new(64 * 1024);
    let wasi = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .inherit_stderr()
        .build();

    let state = cds::DynState::new(wasi, ResourceTable::new(), registry.clone());
    let mut store = Store::new(&engine, state);

    // 3. Drive the guest's wasi:cli/run. It resolves "provider" and invokes
    //    "upper" on "hello from dlopen".
    let command = Command::instantiate(&mut store, &guest_component, &linker).expect("instantiate guest");
    let run_result = command
        .wasi_cli_run()
        .call_run(&mut store)
        .expect("call run");
    assert!(run_result.is_ok(), "guest run() returned an error exit");

    drop(store);
    let out = stdout.contents();
    let out_str = String::from_utf8_lossy(&out);
    eprintln!("=== dlopen guest stdout ===\n{out_str}\n===========================");
    assert!(
        out_str.contains("HELLO FROM DLOPEN"),
        "expected 'HELLO FROM DLOPEN' from the resolved+invoked shared provider, got: {out_str:?}"
    );

    // 4. Shared-copy property: after the guest resolved once, exactly ONE
    //    resident provider instance backs the id. Resolve AGAIN (a second
    //    guest run) and assert it still reuses the SAME single instance.
    assert_eq!(
        registry.resident_count("provider"),
        1,
        "first resolve must have materialized exactly one resident provider"
    );

    let stdout2 = MemoryOutputPipe::new(64 * 1024);
    let wasi2 = WasiCtxBuilder::new()
        .stdout(stdout2.clone())
        .inherit_stderr()
        .build();
    let state2 = cds::DynState::new(wasi2, ResourceTable::new(), registry.clone());
    let mut store2 = Store::new(&engine, state2);
    let command2 =
        Command::instantiate(&mut store2, &guest_component, &linker).expect("instantiate guest 2");
    command2
        .wasi_cli_run()
        .call_run(&mut store2)
        .expect("call run 2")
        .expect("run 2 ok");
    drop(store2);
    let out2 = String::from_utf8_lossy(&stdout2.contents()).into_owned();
    assert!(
        out2.contains("HELLO FROM DLOPEN"),
        "second guest run must also print the uppercased result, got: {out2:?}"
    );

    // STILL exactly one resident provider — the second resolve reused the
    // shared copy rather than instantiating a fresh one. This is the
    // "one heavy provider serving many function components" property.
    assert_eq!(
        registry.resident_count("provider"),
        1,
        "the second resolve must reuse the SINGLE shared resident provider"
    );
    eprintln!("[test] shared-copy confirmed: 2 guest runs, 1 resident provider instance");
}
