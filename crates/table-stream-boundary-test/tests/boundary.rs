//! Cross-boundary integration test for the numstream streaming component — the
//! end-to-end proof for the v3 freeze policy's first additive MINOR (3.1.0).
//!
//! Loads the already-built `artifacts/extensions/numstream.wasm` component and
//! drives its `table-stream-dispatch` exports through the WIT component boundary
//! via wasmtime, proving that FILTER pushdown is honored ACROSS the interface:
//! the neutral filter descriptor (column index + comparator + constant) is
//! encoded into the component's linear memory by the canonical ABI and the
//! component prunes the generated rows AT THE SOURCE — not merely exercised by
//! native Rust logic.
//!
//! Strategy: wasmtime does NOT run a component's `load()` on instantiation, so
//! the imported interfaces (runtime/config/logging/catalog/files/table-stream)
//! are never actually called. We only need to SATISFY them at link time, which we
//! do with trapping stubs. We then call the `table-stream-dispatch` exports
//! directly with handle = 1 (the callback-handle the component registers).

use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "boundary-test",
    });
}

use bindings::duckdb::extension as ext;
use bindings::exports::duckdb::extension::table_stream_dispatch as tsd;
use bindings::BoundaryTest;
use ext::types::Duckvalue;

struct State {
    table: ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
}

impl Default for State {
    fn default() -> Self {
        State {
            table: ResourceTable::new(),
            wasi: wasmtime_wasi::WasiCtxBuilder::new().inherit_stderr().build(),
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

// ---- table-stream (the 3.1.0 additive registration marker) ----
impl ext::table_stream::Host for State {
    fn register_filterable_table(
        &mut self,
        _name: String,
        _arguments: Vec<ext::types::Funcarg>,
        _columns: Vec<ext::types::Columndef>,
        _callback_handle: u32,
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

const HANDLE: u32 = 1;

fn load_component() -> (Store<State>, BoundaryTest) {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");

    let mut linker: Linker<State> = Linker::new(&engine);
    BoundaryTest::add_to_linker::<State, wasmtime::component::HasSelf<State>>(&mut linker, |s| s)
        .expect("add_to_linker");
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("wasi add_to_linker");

    let mut store = Store::new(&engine, State::default());

    let manifest = env!("CARGO_MANIFEST_DIR");
    let wasm_path =
        std::path::Path::new(manifest).join("../../artifacts/extensions/numstream.wasm");
    let bytes = std::fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()));
    assert_eq!(
        &bytes[0..8],
        &[0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00],
        "not a wasm component (bad magic)"
    );
    let component = Component::new(&engine, &bytes).expect("Component::new");
    let instance = BoundaryTest::instantiate(&mut store, &component, &linker).expect("instantiate");
    (store, instance)
}

/// Pull every row from a cursor into a flat Vec of the projected first column.
fn drain(store: &mut Store<State>, td: &tsd::Guest, cursor: u32) -> Vec<i64> {
    let mut out = Vec::new();
    loop {
        let batch = td
            .call_call_table_next(&mut *store, HANDLE, cursor, 3)
            .expect("table_next host call")
            .expect("table_next component result");
        if batch.is_empty() {
            break;
        }
        for row in batch {
            match &row[0] {
                Duckvalue::Int64(v) => out.push(*v),
                other => panic!("expected Int64 across boundary, got {other:?}"),
            }
        }
    }
    out
}

#[test]
fn filter_pushdown_crosses_wit_boundary_and_prunes() {
    let (mut store, instance) = load_component();
    let td = instance.duckdb_extension_table_stream_dispatch();

    // --- baseline: open WITHOUT filters -> all 10 rows (0..9) ---
    let open = td
        .call_call_table_open(&mut store, HANDLE, &[Duckvalue::Int64(10)], &[])
        .expect("table_open host call")
        .expect("table_open component result");
    // schema crosses the boundary: one column `v`.
    assert_eq!(open.columns.len(), 1, "one emitted column across boundary");
    assert_eq!(open.columns[0].name, "v");
    let all = drain(&mut store, &td, open.cursor);
    assert_eq!(all, (0..10).collect::<Vec<i64>>(), "unfiltered scan");
    td.call_call_table_close(&mut store, HANDLE, open.cursor)
        .expect("close host")
        .expect("close result");

    // --- PUSHDOWN: open WITH filter v > 5 -> exactly 6,7,8,9 (pruned at source) ---
    let filters = vec![tsd::TableFilter {
        column: 0,
        op: tsd::FilterOp::Gt,
        values: vec![Duckvalue::Int64(5)],
    }];
    let open2 = td
        .call_call_table_open_filtered(&mut store, HANDLE, &[Duckvalue::Int64(10)], &[], &filters)
        .expect("open_filtered host call")
        .expect("open_filtered component result");
    let kept = drain(&mut store, &td, open2.cursor);
    assert_eq!(
        kept,
        vec![6, 7, 8, 9],
        "the pushed-down filter v > 5 must reach the component and prune at source"
    );
    td.call_call_table_close(&mut store, HANDLE, open2.cursor)
        .expect("close2 host")
        .expect("close2 result");

    // --- conjunction: v > 2 AND v <= 6 -> 3,4,5,6 ---
    let conj = vec![
        tsd::TableFilter {
            column: 0,
            op: tsd::FilterOp::Gt,
            values: vec![Duckvalue::Int64(2)],
        },
        tsd::TableFilter {
            column: 0,
            op: tsd::FilterOp::Le,
            values: vec![Duckvalue::Int64(6)],
        },
    ];
    let open3 = td
        .call_call_table_open_filtered(&mut store, HANDLE, &[Duckvalue::Int64(10)], &[], &conj)
        .expect("open_filtered conj host call")
        .expect("open_filtered conj component result");
    let kept3 = drain(&mut store, &td, open3.cursor);
    assert_eq!(
        kept3,
        vec![3, 4, 5, 6],
        "the pushed-down conjunction must AND at the source"
    );
    td.call_call_table_close(&mut store, HANDLE, open3.cursor)
        .expect("close3 host")
        .expect("close3 result");
}
