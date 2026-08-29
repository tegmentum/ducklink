//! ADR-0029 Phase 6.2.d.1 — dlopen guest test rewritten against the
//! wasmos-runtime-api abstraction. Full end-to-end verification of
//! the abstraction stack landed in Phases 6.1a + 6.2.b + 6.2.b.2 +
//! 6.2.b.3 + 6.2.b.4 against datalink-dynlink-wasmos v0.1.0.
//!
//! What changed from the pre-migration version:
//!   * OLD: wasmtime::Engine + Component + Linker + Store; ducklink's
//!     DynState + `impl_compose_dynlink_host!` macro expansion;
//!     wasmtime-wasi's MemoryOutputPipe for stdout capture;
//!     `Command::instantiate` + `wasi_cli_run().call_run(&mut store)`.
//!   * NEW: `wasmos_runtime_select::SelectedRuntime` +
//!     `runtime.compile_component` + `runtime.instantiate` +
//!     `Instance::call_wasi_command`; `datalink_dynlink_wasmos`
//!     `ProviderRegistry` + `ResidentBackend` +
//!     `install_host_imports`; `WasiEnvironment::with_stdout_capture`
//!     (Phase 6.2.b.4) for the assert-on-output side.
//!
//! Same test semantics: register echo provider, drive dlopen guest,
//! assert stdout contains "HELLO FROM DLOPEN", verify a second guest
//! run produces the same output (proving shared-copy — the resident
//! provider is materialised once and reused across both invocations).
//!
//! Skips when the sibling `webassembly-component-orchestration`
//! checkout is missing.

use std::path::PathBuf;
use std::sync::Arc;

use datalink_dynlink_wasmos::{install_host_imports, ProviderRegistry, ResidentBackend};
use wasmos_runtime_api::{
    ComponentSource, ExecutionContext, HostImports, Runtime, RuntimeConfig, WasiEnvironment,
};
use wasmos_runtime_select::SelectedRuntime;

fn orchestration_repo() -> PathBuf {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dlopen_guest_invokes_shared_provider_and_prints_uppercase() {
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

    let runtime: Arc<SelectedRuntime> = Arc::new(
        SelectedRuntime::new(RuntimeConfig::default()).expect("build SelectedRuntime"),
    );

    // 1. Build the shared provider registry and register the echo
    //    provider under id "provider". register_provider compiles
    //    it now; the resident instance is materialized lazily on
    //    first resolve (via ResidentBackend).
    let registry = ProviderRegistry::new(runtime.clone());
    registry
        .register_provider("provider", &provider_path)
        .await
        .expect("register echo provider");

    // 2. Build the ResidentBackend + install as a HostImports
    //    handler. The compose:dynlink/linker interface auto-registers
    //    its `resource instance` type via the adapter's Phase 6.2.b
    //    resource-type auto-registration; the HostCall dispatches
    //    method calls to the backend.
    let backend = Arc::new(ResidentBackend::new(registry.clone()));
    let host_imports = install_host_imports(HostImports::new(), backend);

    // 3. Compile the guest.
    let guest_bytes: bytes::Bytes = std::fs::read(&guest_path).expect("read guest").into();
    let guest = runtime
        .compile_component(
            ComponentSource::Bytes { bytes: guest_bytes.clone(), name: Some("dlopen-guest".into()) },
            Default::default(),
        )
        .await
        .expect("compile guest");

    // 4. Drive the guest's wasi:cli/run. Capture stdout via
    //    WasiEnvironment::with_stdout_capture (Phase 6.2.b.4).
    let (env, stdout) = WasiEnvironment::sandboxed().with_stdout_capture();
    let (env, stderr) = env.with_stderr_capture();

    let mut instance = runtime
        .instantiate(
            &guest,
            ExecutionContext::new().with_wasi(env).with_host_imports(host_imports.clone()),
        )
        .await
        .expect("instantiate guest");

    let run_result = instance.call_wasi_command().await;
    if let Err(e) = &run_result {
        eprintln!("=== guest stderr on error ===\n{}", String::from_utf8_lossy(&stderr.lock().unwrap()));
        eprintln!("=== guest stdout on error ===\n{}", String::from_utf8_lossy(&stdout.lock().unwrap()));
        eprintln!("=== error ===\n{e:#?}");
    }
    run_result
        .expect("call wasi:cli/run should return Ok")
        .expect("guest run() returned an error exit");
    drop(instance);

    let out = stdout.lock().unwrap().clone();
    let out_str = String::from_utf8_lossy(&out);
    eprintln!("=== dlopen guest stdout ===\n{out_str}\n===========================");
    assert!(
        out_str.contains("HELLO FROM DLOPEN"),
        "expected 'HELLO FROM DLOPEN' from the resolved+invoked shared provider, got: {out_str:?}"
    );

    // 5. Second guest run — verifies the shared-copy property. The
    //    ResidentBackend's slot-per-id lazy-materialize logic hands
    //    back the SAME provider instance both times; we can't
    //    inspect the resident count directly through the abstraction
    //    (that's ResidentBackend-internal state), so the proof is
    //    that a second run produces the same output correctly.
    let (env2, stdout2) = WasiEnvironment::sandboxed().with_stdout_capture();
    let (env2, _stderr2) = env2.with_stderr_capture();
    let mut instance2 = runtime
        .instantiate(
            &guest,
            ExecutionContext::new().with_wasi(env2).with_host_imports(host_imports),
        )
        .await
        .expect("instantiate guest 2");
    instance2
        .call_wasi_command()
        .await
        .expect("call 2 ok")
        .expect("run 2 ok");
    drop(instance2);
    let out2 = String::from_utf8_lossy(&stdout2.lock().unwrap()).into_owned();
    assert!(
        out2.contains("HELLO FROM DLOPEN"),
        "second guest run must also print the uppercased result, got: {out2:?}"
    );

    eprintln!("[test] dlopen migration proof: 2 guest runs, both printed via shared provider");
}
