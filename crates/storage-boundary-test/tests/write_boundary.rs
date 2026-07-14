//! Cross-boundary integration test for the WRITABLE half of the storage
//! contract.
//!
//! Loads the built synthetic writer bridge (from either the sibling repo or
//! the local `artifacts/extensions/` staging dir) and drives its
//! `storage-write-dispatch@2.0.0` exports through the WIT component boundary
//! via wasmtime, proving that transactional / DDL / DML calls round-trip
//! ACROSS the interface: args are encoded into the component's linear memory
//! by the canonical ABI and the resultset (rowids + counts) is decoded back
//! out, not merely exercised by the component's native Rust logic. This is
//! the mirror-image counterpart to `boundary.rs` (which exercises the
//! read-side `storage-dispatch` interface against the sqlite component).
//!
//! Strategy: identical to the read-side test. The bridge is a plain writer
//! component with NO imports (its `writer` world only exports
//! storage-write-dispatch), so we just instantiate it and call the exports
//! directly. `handle = 1` and `catalog = 1` are the fixed values the bridge
//! serves (it does not model a callback registry / catalog table).

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit-write",
        world: "write-boundary-test",
    });
}

use bindings::duckdb::extension as ext;
use bindings::WriteBoundaryTest;
use ext::types::Duckvalue;

// The v0.2.0 bridge's `guest.load()` calls storage.register-storage;
// wasmtime instantiation refuses to bind a component that imports an
// interface the linker cannot satisfy, so provide a no-op host impl for
// every interface the bridge world imports. The write-boundary test never
// actually calls guest.load() -- it drives the storage-write-dispatch
// export directly -- so the host impls are only there so the LINKER's
// satisfaction check passes at instantiate-time.
impl ext::types::Host for State {}
impl ext::storage::Host for State {
    fn register_storage(
        &mut self,
        _type_name: String,
        _callback_handle: u32,
        _options: Option<ext::storage::Extopts>,
    ) -> Result<u32, ext::types::Duckerror> {
        Ok(0)
    }
}

/// Store state. The writer bridge's `writer` world imports nothing from the
/// `duckdb:extension` package, but the wasip2 stdlib pulls in `wasi:io` /
/// `wasi:cli` / `wasi:filesystem` implicitly (Mutex + OnceLock allocate,
/// panic-time stderr goes through wasi-cli); satisfy those with a real (but
/// idle) `WasiCtx` so instantiation succeeds. The dispatch calls never touch
/// them.
struct State {
    table: ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
}

impl Default for State {
    fn default() -> Self {
        State {
            table: ResourceTable::new(),
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
        }
    }
}

impl wasmtime_wasi::WasiView for State {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

const HANDLE: u32 = 1;
const CATALOG: u32 = 1;

fn locate_writer_bridge_wasm() -> std::path::PathBuf {
    // 1. explicit env var wins (CI).
    if let Ok(p) = std::env::var("DUCKLINK_WRITER_BRIDGE_WASM") {
        return std::path::PathBuf::from(p);
    }
    // 2. staged copy in the local artifacts dir (developer convenience:
    //    `cp .../synthetic_mutating_ducklink_bridge_dynlink.wasm
    //     ducklink/artifacts/extensions/synthetic_mutating_ducklink.wasm`).
    let manifest = env!("CARGO_MANIFEST_DIR");
    let staged = std::path::Path::new(manifest)
        .join("../../artifacts/extensions/synthetic_mutating_ducklink.wasm");
    if staged.exists() {
        return staged;
    }
    // 3. sibling checkout (default dev layout: ~/git/synthetic-mutating-vtab-
    //    ducklink-bridge/ next to ~/git/ducklink/).
    let home = std::env::var("HOME").expect("HOME env var");
    let sibling = std::path::PathBuf::from(home).join(
        "git/synthetic-mutating-vtab-ducklink-bridge/target/wasm32-wasip2/release/\
         synthetic_mutating_ducklink_bridge_dynlink.wasm",
    );
    if sibling.exists() {
        return sibling;
    }
    panic!(
        "writer bridge wasm not found. tried:\n\
        \x20 * $DUCKLINK_WRITER_BRIDGE_WASM (unset)\n\
        \x20 * {}\n\
        \x20 * {}\n\
        build the bridge with:\n\
        \x20 (cd ~/git/synthetic-mutating-vtab-ducklink-bridge && \
        cargo build --target wasm32-wasip2 --release)\n\
        then either set DUCKLINK_WRITER_BRIDGE_WASM=<path> or copy the wasm to \
        the staged location.",
        staged.display(),
        sibling.display(),
    );
}

fn columns_kv() -> Vec<ext::types::Columndef> {
    vec![
        ext::types::Columndef {
            name: "key".to_string(),
            logical: ext::types::Logicaltype::Text,
        },
        ext::types::Columndef {
            name: "value".to_string(),
            logical: ext::types::Logicaltype::Text,
        },
    ]
}

fn row(k: &str, v: &str) -> Vec<Duckvalue> {
    vec![Duckvalue::Text(k.to_string()), Duckvalue::Text(v.to_string())]
}

fn setup() -> (Store<State>, WriteBoundaryTest) {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");

    // The v0.2.0 bridge's `writer` world imports `duckdb:extension/storage`
    // (needed by guest.load's register-storage call) plus the wasip2 stdlib's
    // wasi:io / wasi:cli / wasi:filesystem. Bind both: WriteBoundaryTest's
    // bindgen-generated add_to_linker wires the storage host trait we
    // implemented above, and wasmtime_wasi covers the wasi side.
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("wasi add_to_linker");
    WriteBoundaryTest::add_to_linker::<State, wasmtime::component::HasSelf<State>>(
        &mut linker,
        |s| s,
    )
    .expect("write-boundary add_to_linker");
    let mut store = Store::new(&engine, State::default());

    let wasm_path = locate_writer_bridge_wasm();
    let bytes = std::fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()));
    assert_eq!(
        &bytes[0..8],
        &[0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00],
        "not a wasm component (bad magic) at {}",
        wasm_path.display()
    );
    let component = Component::new(&engine, &bytes).expect("Component::new");
    let instance =
        WriteBoundaryTest::instantiate(&mut store, &component, &linker).expect("instantiate");
    (store, instance)
}

/// Full happy path: begin -> create -> insert -> update -> delete -> commit.
/// Every dispatch call round-trips through the canonical ABI; the assertions
/// check the returned counts / rowids the component emits.
#[test]
fn write_dispatch_crosses_wit_boundary() {
    let (mut store, instance) = setup();
    let wd = instance.duckdb_extension_storage_write_dispatch();

    // begin_transaction across the boundary.
    let txn = wd
        .call_begin_transaction(&mut store, HANDLE, CATALOG)
        .expect("begin_transaction host call")
        .expect("begin_transaction component result");
    assert!(txn >= 1, "txn handle must be non-zero, got {txn}");

    // create_table across the boundary.
    let cols = columns_kv();
    wd.call_create_table(&mut store, HANDLE, txn, "kv_store", &cols)
        .expect("create_table host call")
        .expect("create_table component result");

    // insert_rows across the boundary. Two rows, count=2.
    let rows = vec![row("alpha", "1"), row("beta", "2")];
    let inserted = wd
        .call_insert_rows(&mut store, HANDLE, txn, "kv_store", &rows)
        .expect("insert_rows host call")
        .expect("insert_rows component result");
    assert_eq!(inserted, 2, "insert returned {inserted}, want 2");

    // update_rows across the boundary. The bridge assigns rowids 1..=N in
    // monotonic order; update rowid 1 in place, count=1.
    let update_row = vec![row("alpha", "one")];
    let updated = wd
        .call_update_rows(
            &mut store,
            HANDLE,
            txn,
            "kv_store",
            &[1_i64],
            &update_row,
        )
        .expect("update_rows host call")
        .expect("update_rows component result");
    assert_eq!(updated, 1, "update returned {updated}, want 1");

    // delete_rows across the boundary. Delete rowid 2, count=1.
    let deleted = wd
        .call_delete_rows(&mut store, HANDLE, txn, "kv_store", &[2_i64])
        .expect("delete_rows host call")
        .expect("delete_rows component result");
    assert_eq!(deleted, 1, "delete returned {deleted}, want 1");

    // commit_transaction across the boundary.
    wd.call_commit_transaction(&mut store, HANDLE, txn)
        .expect("commit_transaction host call")
        .expect("commit_transaction component result");
}

/// Rollback path: begin -> insert -> rollback. After rollback, a new txn
/// starts from the pre-rollback committed state (the bridge's committed
/// state was already populated by `write_dispatch_crosses_wit_boundary`,
/// so this test starts with a fresh setup and checks that a rollback
/// leaves no writes visible in the next txn).
#[test]
fn write_dispatch_rollback_crosses_wit_boundary() {
    let (mut store, instance) = setup();
    let wd = instance.duckdb_extension_storage_write_dispatch();

    // begin_transaction + create_table + insert 3 rows.
    let txn = wd
        .call_begin_transaction(&mut store, HANDLE, CATALOG)
        .expect("begin_transaction host call")
        .expect("begin_transaction component result");
    let cols = columns_kv();
    wd.call_create_table(&mut store, HANDLE, txn, "kv_store", &cols)
        .expect("create_table host call")
        .expect("create_table component result");
    let rows = vec![row("k1", "v1"), row("k2", "v2"), row("k3", "v3")];
    let inserted = wd
        .call_insert_rows(&mut store, HANDLE, txn, "kv_store", &rows)
        .expect("insert_rows host call")
        .expect("insert_rows component result");
    assert_eq!(inserted, 3, "insert returned {inserted}, want 3");

    // rollback_transaction across the boundary.
    wd.call_rollback_transaction(&mut store, HANDLE, txn)
        .expect("rollback_transaction host call")
        .expect("rollback_transaction component result");

    // Open a new txn and delete a rowid the rolled-back txn had inserted.
    // The rollback should have discarded the shadow, so the delete of a
    // never-committed rowid returns 0 (nothing to delete).
    let txn2 = wd
        .call_begin_transaction(&mut store, HANDLE, CATALOG)
        .expect("begin_transaction (rerun) host call")
        .expect("begin_transaction (rerun) component result");
    let deleted = wd
        .call_delete_rows(&mut store, HANDLE, txn2, "kv_store", &[1_i64, 2_i64, 3_i64])
        .expect("delete_rows (post-rollback) host call")
        .expect("delete_rows (post-rollback) component result");
    assert_eq!(
        deleted, 0,
        "post-rollback delete returned {deleted}, want 0 (shadow was discarded)"
    );
    wd.call_rollback_transaction(&mut store, HANDLE, txn2)
        .expect("cleanup rollback host call")
        .expect("cleanup rollback component result");
}
