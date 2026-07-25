//! `nested-exec` Direction-1 §6 re-entrancy PoC.
//!
//! Empirical probe of the two "walls" `docs/nested-exec-direction-1-plan.md`
//! §1.3 cites as gating option (a) (thread `StoreContextMut` through the
//! callback path) and option (c) (add a core-WIT sibling-connect surface).
//!
//! We answer the two independent questions:
//!
//!   1. **Mutex wall.** With the primary core wrapped in
//!      `Arc<Mutex<CoreExecution>>` and the outer `Manager::with_core` holding
//!      the guard for the duration of `guest.call_execute`, does a callback
//!      firing INSIDE that call re-acquire the mutex? (Answer: no — it
//!      deadlocks / `try_lock` -> `WouldBlock`. This is a `std::sync::Mutex`
//!      documented invariant, verified below on the wrapper shape.)
//!
//!   2. **Wasmtime store wall.** Does wasmtime's runtime forbid a host
//!      function invoked BY a store mid-call from calling BACK into another
//!      export on the same store? (Answer: **NO — wasmtime supports it** when
//!      the host function has a `Caller<'_, T>` in hand. This is a
//!      well-supported pattern; see the wasmtime docs on host-function
//!      recursion. The `Store` "in use" invariant is not `!Reentrant`; it is
//!      "at most one exclusive borrow of the store at a time," and a
//!      `Caller<'_, T>` inside a host callback IS that borrow, from which
//!      further calls are permitted.)
//!
//! **Combined finding.** The wasmtime primitive supports re-entry. The
//! blockers on the ducklink side are:
//!
//!   * The `Arc<Mutex<CoreExecution>>` wrapper is non-reentrant. The current
//!     `Manager::with_core` (crates/ducklink-host/src/lib.rs:4254) locks it
//!     for the whole `call_execute`, so a callback cannot re-acquire it.
//!   * The bindgen `Host` traits deliver `&mut CoreStoreState` — the store
//!     *data*, not the store *context*. The callback has no `Caller<'_, T>`
//!     / `StoreContextMut<'_, T>` reachable from `&mut T`. That is the
//!     "plumbing overhaul" cost of option (a) — see the memo §7 for the
//!     refined estimate.
//!
//! The failure mode of the current (b.1) sibling-core impl is reproduced
//! empirically via the standalone CLI (see the memo §7 "PoC findings" block
//! for the exact commands and observed output).

use anyhow::Result;
use std::sync::{Arc, Mutex, TryLockError};
use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Store};

/// Wall #1 (mutex non-reentrancy) — direct verification.
///
/// The wrapper shape is `Arc<Mutex<CoreExecution>>`. `std::sync::Mutex` is
/// documented as non-reentrant. Holding the guard, `try_lock` from the same
/// thread must return `WouldBlock`; a blocking `lock()` would deadlock (we
/// do not test the deadlock path — it would hang the test — but the failure
/// mode is symmetric).
#[test]
fn wall1_std_mutex_is_non_reentrant() {
    let core: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let outer = core.lock().expect("outer lock");
    let inner = core.try_lock();
    let verdict = match inner {
        Err(TryLockError::WouldBlock) => Ok(()),
        Err(TryLockError::Poisoned(_)) => Err("mutex was poisoned"),
        Ok(_) => Err(
            "std::sync::Mutex unexpectedly permitted re-entry; ducklink's re-entrancy \
             analysis (RE-ENTRANCY GUARD comment at lib.rs:5760) would be wrong",
        ),
    };
    drop(inner);
    drop(outer);
    verdict.expect("wall #1 mutex re-entry verdict");
}

/// Wall #2 (wasmtime store re-entry) — empirical result.
///
/// This is the memo §6 "wasmtime reentrancy PoC" the task asked for. A wasm
/// module exports `run` and `sink`; `run` calls a host import `cb`; the host
/// implementation of `cb` uses its `Caller<'_, T>` to fetch `sink` from the
/// same instance and invoke it — RE-ENTERING the same store while `run` is
/// still on the stack.
///
/// If wasmtime forbade this, the nested `Func::call` would trap with a
/// "store is in use" (or "reentrant call") error. The test asserts the
/// opposite: **wasmtime accepts it**, provided the host callback has a
/// `Caller<'_, T>` (equivalently `StoreContextMut<'_, T>`) in hand.
#[test]
fn wall2_wasmtime_permits_reentry_from_host_callback_with_caller() -> Result<()> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(false);
    let engine = Engine::new(&cfg)?;
    // Two exports (`run`, `sink`) + one host import (`host.cb`). `run` calls
    // `cb`; the host `cb` uses the `Caller<'_, T>` it received to call
    // `sink` on the same instance, which mutates a store-owned counter so
    // we can see the nested call actually executed.
    let wat = r#"
        (module
          (import "host" "cb" (func $cb))
          (func (export "run") call $cb)
          (func (export "sink") nop)
        )
    "#;
    let bytes = wat::parse_str(wat)?;
    let module = Module::new(&engine, bytes)?;

    // Store state: a counter the host callback bumps AFTER the nested call
    // returns, so we can observe control-flow ordering.
    #[derive(Default)]
    struct StoreData {
        nested_call_ok: bool,
        nested_call_err: Option<String>,
    }
    let mut store: Store<StoreData> = Store::new(&engine, StoreData::default());
    let mut linker: Linker<StoreData> = Linker::new(&engine);

    linker.func_wrap("host", "cb", |mut caller: Caller<'_, StoreData>| {
        // Fetch the same instance's `sink` export via the caller's context
        // and invoke it. This is the RE-ENTRANT call the memo §1.3 posits
        // as forbidden.
        let sink = match caller
            .get_export("sink")
            .and_then(|e| e.into_func())
        {
            Some(f) => f,
            None => {
                caller.data_mut().nested_call_err =
                    Some("sink export missing".to_string());
                return;
            }
        };
        match sink.call(&mut caller, &[], &mut []) {
            Ok(()) => caller.data_mut().nested_call_ok = true,
            Err(e) => caller.data_mut().nested_call_err = Some(e.to_string()),
        }
    })?;

    let instance: Instance = linker.instantiate(&mut store, &module)?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    run.call(&mut store, ())?;

    let data = store.data();
    assert!(
        data.nested_call_err.is_none(),
        "wasmtime rejected re-entry from a host callback holding Caller<'_, T>: {:?} \
         — this would VALIDATE memo §1.3 wall #2. Currently the memo overstates the \
         wasmtime constraint if this fires.",
        data.nested_call_err
    );
    assert!(
        data.nested_call_ok,
        "nested call reported no error but did not run; store state was not updated"
    );
    Ok(())
}

/// Wall #2 counter-experiment: DIFFERENT store, same engine.
///
/// Confirms the "in use" invariant is per-store, not per-engine — orthogonal
/// evidence for wall #2: wasmtime happily runs a call on a second, unrelated
/// store while `run` is mid-flight on the first. Included to distinguish
/// "store re-entry forbidden globally" (which would also affect this test)
/// from "at most one exclusive borrow per store" (the actual invariant).
#[test]
fn wall2_wasmtime_permits_calls_on_a_second_store_same_engine() -> Result<()> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(false);
    let engine = Engine::new(&cfg)?;
    let wat = r#"
        (module
          (import "host" "cb" (func $cb))
          (func (export "run") call $cb)
          (func (export "sibling") nop)
        )
    "#;
    let bytes = wat::parse_str(wat)?;
    let module = Module::new(&engine, bytes)?;

    // Two independent stores sharing the engine. `store_b` is the "sibling"
    // in the memo's terminology — mimics what (b.1) does today (second
    // `CoreExecution`).
    let store_a: Arc<Mutex<Store<()>>> = Arc::new(Mutex::new(Store::new(&engine, ())));
    let store_b: Arc<Mutex<Store<()>>> = Arc::new(Mutex::new(Store::new(&engine, ())));

    // Instantiate B once so the callback can use it. A gets instantiated
    // after the linker is built.
    let instance_b = {
        let mut b_guard = store_b.lock().expect("store_b lock");
        let mut linker_b: Linker<()> = Linker::new(&engine);
        linker_b.func_wrap("host", "cb", |_c: Caller<'_, ()>| {})?;
        linker_b.instantiate(&mut *b_guard, &module)?
    };

    let mut linker_a: Linker<()> = Linker::new(&engine);
    let store_b_for_cb = store_b.clone();
    // Returning `wasmtime::Result<()>` (== `anyhow::Result<()>`) so wasmtime's
    // `IntoFunc` derives the error path.
    linker_a.func_wrap(
        "host",
        "cb",
        move |_caller: Caller<'_, ()>| -> wasmtime::Result<()> {
            // Try to call into store_b while store_a is mid-`run`. In the
            // ducklink codebase this maps to (b.1): the primary is mid-call,
            // the callback runs SQL against the sibling core. Different
            // store => different exclusive borrow => wasmtime allows it.
            let mut b_guard = store_b_for_cb.lock().expect("store_b lock");
            let sibling = instance_b.get_typed_func::<(), ()>(&mut *b_guard, "sibling")?;
            sibling.call(&mut *b_guard, ())?;
            Ok(())
        },
    )?;

    let instance_a = {
        let mut a_guard = store_a.lock().expect("store_a lock");
        linker_a.instantiate(&mut *a_guard, &module)?
    };
    {
        let mut a_guard = store_a.lock().expect("store_a lock");
        let run = instance_a.get_typed_func::<(), ()>(&mut *a_guard, "run")?;
        run.call(&mut *a_guard, ())?;
    }
    Ok(())
}
