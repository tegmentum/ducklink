//! Cross-boundary integration test for the sqlite storage component.
//!
//! Loads the already-built `artifacts/extensions/sqlitewasm.wasm` component and
//! drives its `storage-dispatch` exports through the WIT component boundary via
//! wasmtime, proving that projection + filter pushdown is honored ACROSS the
//! interface: the args are encoded into the component's linear memory by the
//! canonical ABI and the resultset is decoded back out — not merely exercised
//! by the component's native Rust logic.
//!
//! Strategy: wasmtime does NOT run a component's `load()` on instantiation, so
//! the imported interfaces (runtime/config/logging/catalog/files/storage) are
//! never actually called. We only need to SATISFY them at link time, which we
//! do with trapping stubs. We then call the `storage-dispatch` exports directly
//! with handle = 1 (the backend `callback-handle` the component registers).

use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "boundary-test",
    });
}

use bindings::duckdb::extension as ext;
use bindings::BoundaryTest;
use ext::types::Duckvalue;

/// Store state. Holds a `ResourceTable` so imported interfaces that declare
/// resources (runtime, catalog) have somewhere to live — even though we never
/// construct any of those resources because the imports are never called — plus
/// a real `WasiCtx`, since the sqlite component is a wasip2 component (std +
/// rusqlite) and imports the `wasi:cli/filesystem/io` interfaces.
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

macro_rules! unreachable_import {
    () => {
        unreachable!(
            "imported host function called, but the boundary test never invokes guest.load"
        )
    };
}

// ---- types (marker trait: no functions) ----
impl ext::types::Host for State {}

// ---- logging ----
impl ext::logging::Host for State {
    fn log(&mut self, _level: ext::types::Loglevel, _message: String, _target: Option<String>) {
        unreachable_import!()
    }
    fn log_fields(
        &mut self,
        _level: ext::types::Loglevel,
        _message: String,
        _fields: Vec<ext::types::Logfield>,
    ) {
        unreachable_import!()
    }
}

// ---- config ----
impl ext::config::Host for State {
    fn provider_version(&mut self) -> String {
        unreachable_import!()
    }
    fn list_keys(&mut self, _prefix: Option<String>) -> Vec<String> {
        unreachable_import!()
    }
    fn get_string(&mut self, _path: String) -> Result<Option<String>, ext::types::Configerror> {
        unreachable_import!()
    }
    fn get_bool(&mut self, _path: String) -> Result<Option<bool>, ext::types::Configerror> {
        unreachable_import!()
    }
    fn get_i64(&mut self, _path: String) -> Result<Option<i64>, ext::types::Configerror> {
        unreachable_import!()
    }
    fn get_u64(&mut self, _path: String) -> Result<Option<u64>, ext::types::Configerror> {
        unreachable_import!()
    }
    fn get_f64(&mut self, _path: String) -> Result<Option<f64>, ext::types::Configerror> {
        unreachable_import!()
    }
    fn get_bytes(&mut self, _path: String) -> Result<Option<Vec<u8>>, ext::types::Configerror> {
        unreachable_import!()
    }
    fn get_string_list(
        &mut self,
        _path: String,
    ) -> Result<Option<Vec<String>>, ext::types::Configerror> {
        unreachable_import!()
    }
}

// ---- files ----
impl ext::files::Host for State {
    fn register_replacement_scan(
        &mut self,
        _scan: ext::files::ReplacementScan,
    ) -> Result<ext::files::ReplacementScanId, String> {
        unreachable_import!()
    }
    fn register_copy_handler(
        &mut self,
        _handler: ext::files::CopyHandler,
    ) -> Result<ext::files::CopyHandlerId, String> {
        unreachable_import!()
    }
}

// ---- storage (free fn) ----
impl ext::storage::Host for State {
    fn register_storage(
        &mut self,
        _type_name: String,
        _callback_handle: u32,
        _options: Option<ext::types::Extopts>,
    ) -> Result<u32, ext::types::Duckerror> {
        unreachable_import!()
    }
}

// ---- runtime: free fns + resources (each resource needs a Host* trait) ----
impl ext::runtime::Host for State {
    fn get_capability(
        &mut self,
        _kind: ext::types::Capabilitykind,
    ) -> Option<ext::runtime::Capability> {
        unreachable_import!()
    }
    fn list_capabilities(&mut self) -> Vec<ext::types::Capabilitykind> {
        unreachable_import!()
    }
}

impl ext::runtime::HostScalarCallback for State {
    fn new(&mut self, _handle: u32) -> Resource<ext::runtime::ScalarCallback> {
        unreachable_import!()
    }
    fn call(
        &mut self,
        _self_: Resource<ext::runtime::ScalarCallback>,
        _args: Vec<Duckvalue>,
        _ctx: ext::types::Invokeinfo,
    ) -> Result<Duckvalue, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::ScalarCallback>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostTableCallback for State {
    fn new(&mut self, _handle: u32) -> Resource<ext::runtime::TableCallback> {
        unreachable_import!()
    }
    fn call(
        &mut self,
        _self_: Resource<ext::runtime::TableCallback>,
        _args: Vec<Duckvalue>,
    ) -> Result<ext::types::Resultset, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::TableCallback>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostAggregateCallback for State {
    fn new(&mut self, _handle: u32) -> Resource<ext::runtime::AggregateCallback> {
        unreachable_import!()
    }
    fn call(
        &mut self,
        _self_: Resource<ext::runtime::AggregateCallback>,
        _rows: ext::types::Rowbatch,
    ) -> Result<Duckvalue, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::AggregateCallback>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostPragmaCallback for State {
    fn new(&mut self, _handle: u32) -> Resource<ext::runtime::PragmaCallback> {
        unreachable_import!()
    }
    fn call(
        &mut self,
        _self_: Resource<ext::runtime::PragmaCallback>,
        _args: Vec<Duckvalue>,
    ) -> Result<Option<Duckvalue>, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::PragmaCallback>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostCastCallback for State {
    fn new(&mut self, _handle: u32) -> Resource<ext::runtime::CastCallback> {
        unreachable_import!()
    }
    fn call(
        &mut self,
        _self_: Resource<ext::runtime::CastCallback>,
        _value: Duckvalue,
    ) -> Result<Duckvalue, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::CastCallback>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostScalarRegistry for State {
    fn register(
        &mut self,
        _self_: Resource<ext::runtime::ScalarRegistry>,
        _name: String,
        _arguments: Vec<ext::types::Funcarg>,
        _returns: ext::types::Logicaltype,
        _callback: Resource<ext::runtime::ScalarCallback>,
        _options: Option<ext::types::Funcopts>,
    ) -> Result<u32, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::ScalarRegistry>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostTableRegistry for State {
    fn register(
        &mut self,
        _self_: Resource<ext::runtime::TableRegistry>,
        _name: String,
        _arguments: Vec<ext::types::Funcarg>,
        _columns: Vec<ext::types::Columndef>,
        _callback: Resource<ext::runtime::TableCallback>,
        _options: Option<ext::types::Extopts>,
    ) -> Result<u32, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::TableRegistry>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostAggregateRegistry for State {
    fn register(
        &mut self,
        _self_: Resource<ext::runtime::AggregateRegistry>,
        _name: String,
        _arguments: Vec<ext::types::Funcarg>,
        _returns: ext::types::Logicaltype,
        _callback: Resource<ext::runtime::AggregateCallback>,
        _options: Option<ext::types::Funcopts>,
    ) -> Result<u32, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::AggregateRegistry>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostPragmaRegistry for State {
    fn register_call(
        &mut self,
        _self_: Resource<ext::runtime::PragmaRegistry>,
        _name: String,
        _arguments: Vec<ext::types::Funcarg>,
        _returns: ext::types::Logicaltype,
        _callback: Resource<ext::runtime::PragmaCallback>,
        _options: Option<ext::types::Extopts>,
    ) -> Result<u32, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::PragmaRegistry>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

impl ext::runtime::HostMacroRegistry for State {
    fn register_scalar(
        &mut self,
        _self_: Resource<ext::runtime::MacroRegistry>,
        _name: String,
        _parameters: Vec<String>,
        _body_sql: String,
        _options: Option<ext::types::Extopts>,
    ) -> Result<bool, ext::types::Duckerror> {
        unreachable_import!()
    }
    fn drop(&mut self, _rep: Resource<ext::runtime::MacroRegistry>) -> wasmtime::Result<()> {
        unreachable_import!()
    }
}

// ---- catalog: free fns (uses runtime.cast-callback resource) ----
impl ext::catalog::Host for State {
    fn register_logical_type(
        &mut self,
        _ty: ext::catalog::LogicalType,
    ) -> Result<ext::catalog::LogicalTypeHandle, String> {
        unreachable_import!()
    }
    fn register_cast(
        &mut self,
        _spec: ext::catalog::CastSpec,
        _callback: Resource<ext::runtime::CastCallback>,
    ) -> Result<(), String> {
        unreachable_import!()
    }
    fn register_macro(&mut self, _def: ext::catalog::MacroDef) -> Result<(), String> {
        unreachable_import!()
    }
}

// ---------------------------------------------------------------------------
// Build a sample SQLite DB natively (same approach as the component's own
// tests: bundled SQLite + raw sqlite3_serialize FFI, since the rusqlite
// `serialize` feature is off).
// ---------------------------------------------------------------------------
fn sample_db_bytes() -> Vec<u8> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(a INTEGER, b TEXT); INSERT INTO t VALUES (1,'x'),(2,'y');")
        .unwrap();
    unsafe {
        let db = conn.handle();
        let mut len: i64 = 0;
        let p = libsqlite3_sys::sqlite3_serialize(
            db,
            b"main\0".as_ptr() as *const _,
            &mut len as *mut i64,
            0,
        );
        assert!(!p.is_null(), "sqlite3_serialize returned null");
        let out = std::slice::from_raw_parts(p as *const u8, len as usize).to_vec();
        libsqlite3_sys::sqlite3_free(p as *mut _);
        out
    }
}

const HANDLE: u32 = 1;

#[test]
fn pushdown_crosses_wit_boundary() {
    // --- engine / linker with trapping import stubs ---
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");

    let mut linker: Linker<State> = Linker::new(&engine);
    // Satisfy every imported interface. add_to_linker for the world wires up all
    // imports (runtime/config/logging/catalog/files/storage) from our impls.
    BoundaryTest::add_to_linker::<State, wasmtime::component::HasSelf<State>>(&mut linker, |s| s)
        .expect("add_to_linker");
    // Satisfy the component's wasi:cli/filesystem/io imports (it is a wasip2
    // component). These are linked but the dispatch calls never touch them.
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("wasi add_to_linker");

    let mut store = Store::new(&engine, State::default());

    // --- load the already-built component artifact ---
    let manifest = env!("CARGO_MANIFEST_DIR");
    let wasm_path = std::path::Path::new(manifest)
        .join("../../artifacts/extensions/sqlitewasm.wasm");
    let bytes = std::fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()));
    assert_eq!(
        &bytes[0..8],
        &[0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00],
        "not a wasm component (bad magic)"
    );
    let component = Component::new(&engine, &bytes).expect("Component::new");

    let instance =
        BoundaryTest::instantiate(&mut store, &component, &linker).expect("instantiate");

    let sd = instance.duckdb_extension_storage_dispatch();

    // --- stage the DB blob across the boundary, then attach ---
    let db = sample_db_bytes();
    sd.call_attach_blob(&mut store, HANDLE, "d", &db)
        .expect("attach_blob host call")
        .expect("attach_blob component result");

    let cat = sd
        .call_storage_attach(&mut store, HANDLE, "d", &[])
        .expect("storage_attach host call")
        .expect("storage_attach component result");

    // --- columns: assert table `t` has columns a, b ---
    let cols = sd
        .call_storage_table_columns(&mut store, HANDLE, cat, "t")
        .expect("table_columns host call")
        .expect("table_columns component result");
    let col_names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(col_names, vec!["a", "b"], "column names across boundary");

    // --- PUSHDOWN scan: projection [0], filter a > 1 -> exactly one row [2] ---
    let req = ext::storage::ScanRequest {
        table: "t".to_string(),
        projection: vec![0],
        filters: vec![ext::storage::ScanFilter {
            column: 0,
            op: ext::storage::CompareOp::Gt,
            value: Duckvalue::Int64(1),
        }],
        limit: None,
    };
    let scan = sd
        .call_storage_scan_open(&mut store, HANDLE, cat, &req)
        .expect("scan_open host call")
        .expect("scan_open component result");
    let rows = sd
        .call_storage_scan_next(&mut store, HANDLE, scan, 100)
        .expect("scan_next host call")
        .expect("scan_next component result");

    assert_eq!(rows.len(), 1, "pushdown filter must keep exactly one row");
    assert_eq!(rows[0].len(), 1, "projection must keep exactly one column");
    match &rows[0][0] {
        Duckvalue::Int64(v) => assert_eq!(*v, 2, "the surviving projected cell is a=2"),
        other => panic!("expected Int64(2) across boundary, got {other:?}"),
    }
    sd.call_storage_scan_close(&mut store, HANDLE, scan)
        .expect("scan_close host call")
        .expect("scan_close component result");

    // --- FULL scan: empty projection + no filters -> two rows, two columns ---
    let full = ext::storage::ScanRequest {
        table: "t".to_string(),
        projection: vec![],
        filters: vec![],
        limit: None,
    };
    let scan2 = sd
        .call_storage_scan_open(&mut store, HANDLE, cat, &full)
        .expect("full scan_open host call")
        .expect("full scan_open component result");
    let all = sd
        .call_storage_scan_next(&mut store, HANDLE, scan2, 100)
        .expect("full scan_next host call")
        .expect("full scan_next component result");
    assert_eq!(all.len(), 2, "full scan must return both rows");
    for row in &all {
        assert_eq!(row.len(), 2, "full scan rows have both columns");
    }
    sd.call_storage_scan_close(&mut store, HANDLE, scan2)
        .expect("full scan_close host call")
        .expect("full scan_close component result");

    sd.call_storage_detach(&mut store, HANDLE, cat)
        .expect("detach host call")
        .expect("detach component result");
}
