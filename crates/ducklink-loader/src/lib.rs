// No-op loader for the standalone (wac-composed) deployment.
//
// The core component imports three interfaces so the native host can load
// extension *components* at runtime: `host-extension-loader` (request a load),
// `extension-loader-hooks` (drain captured registrations), and
// `callback-dispatch` (invoke an extension's callbacks). A standalone wasm has
// no component runtime inside it, so it cannot instantiate extensions; this
// stub satisfies those imports with declining/empty implementations. The
// dispatch entry points are unreachable in practice — nothing ever registers a
// callback handle — but must exist to type-check the composition.
#[allow(warnings)]
mod bindings;

use bindings::exports::duckdb::component::extension_loader_hooks::{
    Guest as HooksGuest, PendingRegistrations,
};
use bindings::exports::duckdb::component::host_extension_loader::Guest as LoaderGuest;
use bindings::exports::duckdb::extension::callback_dispatch::{
    Colvec, Duckerror, Duckvalue, Guest as DispatchGuest, Invokeinfo, Resultset,
};
// The eight host->core callback interfaces (extension-registered backends).
use bindings::exports::duckdb::extension::{
    collation_host, files_host, index_host, optimizer_host, parser_host, pragma_host,
    storage_host, table_stream_host,
};
use bindings::exports::tvm::memory::bytes::Guest as TvmBytesGuest;
use bindings::exports::tvm::memory::manager::{
    Guest as TvmManagerGuest, Handle, RegionInfo, RegionKind, TvmError,
};

struct Component;

impl LoaderGuest for Component {
    fn request_load(_name: String) -> bool {
        // No extensions are linkable in a standalone build.
        false
    }
}

impl HooksGuest for Component {
    fn get_pending_registrations() -> PendingRegistrations {
        PendingRegistrations {
            scalars: Vec::new(),
            tables: Vec::new(),
            aggregates: Vec::new(),
            macros: Vec::new(),
            replacement_scans: Vec::new(),
            logical_types: Vec::new(),
            casts: Vec::new(),
        }
    }
}

fn unreachable_dispatch() -> Duckerror {
    Duckerror::Internal("standalone build has no loadable extensions".to_string())
}

impl DispatchGuest for Component {
    fn call_scalar(
        _handle: u32,
        _args: Vec<Duckvalue>,
        _ctx: Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }

    // HOT PATH: columnar scalar/aggregate/cast (major-4). Unreachable in a
    // standalone build (nothing registers a handle), but must type-check.
    fn call_scalar_batch_col(
        _handle: u32,
        _args: Vec<Colvec>,
        _ctx: Invokeinfo,
    ) -> Result<Colvec, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_aggregate_col(_handle: u32, _args: Vec<Colvec>) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_cast_col(_handle: u32, _arg: Colvec) -> Result<Colvec, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_table(_handle: u32, _args: Vec<Duckvalue>) -> Result<Resultset, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<Duckvalue>,
    ) -> Result<Option<Duckvalue>, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_cast(_handle: u32, _value: Duckvalue) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }
}

// --- Host->core backend callbacks (major-4). ---------------------------------
// The core imports these so extension-registered backends can service ATTACH
// storage, vector indexes, collations, pragmas, parsers, optimizer rules, remote
// files, and filtered table streams. A standalone registers no backends, so each
// `*-list` enumerator returns empty (the core registers nothing and its built-in
// local/in-memory functionality is unaffected) and every per-handle operation is
// unreachable — nothing ever hands out a handle to call back on.

impl storage_host::Guest for Component {
    fn storage_list_types() -> Vec<String> {
        Vec::new()
    }
    fn storage_attach(_dsn: String) -> Result<u32, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_list_tables(_catalog: u32) -> Result<Vec<String>, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_table_columns(
        _catalog: u32,
        _table: String,
    ) -> Result<Vec<storage_host::Columndef>, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_scan_open(
        _catalog: u32,
        _request: storage_host::ScanRequest,
    ) -> Result<u32, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_scan_next(
        _scan: u32,
        _max_rows: u32,
    ) -> Result<storage_host::Resultset, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_scan_close(_scan: u32) -> Result<bool, Duckerror> {
        Err(unreachable_dispatch())
    }
}

impl index_host::Guest for Component {
    fn index_type_list() -> Vec<String> {
        Vec::new()
    }
    fn index_create(
        _type_name: String,
        _index_name: String,
        _dims: u32,
    ) -> Result<u32, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn index_append(
        _handle: u32,
        _rowids: Vec<i64>,
        _vectors: Vec<Vec<f32>>,
    ) -> Result<(), Duckerror> {
        Err(unreachable_dispatch())
    }
    fn index_build(_handle: u32) -> Result<(), Duckerror> {
        Err(unreachable_dispatch())
    }
    fn index_search(
        _handle: u32,
        _query: Vec<f32>,
        _k: u32,
    ) -> Result<Vec<index_host::IndexHit>, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn index_drop(_handle: u32) -> Result<(), Duckerror> {
        Err(unreachable_dispatch())
    }
}

impl collation_host::Guest for Component {
    fn collation_list() -> Vec<collation_host::CollationSpec> {
        Vec::new()
    }
}

impl pragma_host::Guest for Component {
    fn pragma_list() -> Vec<pragma_host::PragmaSpec> {
        Vec::new()
    }
}

impl parser_host::Guest for Component {
    fn parser_list() -> Vec<parser_host::ParserSpec> {
        Vec::new()
    }
    fn call_parse(_handle: u32, _query: String) -> Result<Option<String>, Duckerror> {
        Err(unreachable_dispatch())
    }
}

impl optimizer_host::Guest for Component {
    fn optimizer_list() -> Vec<optimizer_host::OptimizerSpec> {
        Vec::new()
    }
    fn call_optimize(_handle: u32, _plan_json: String) -> Result<Option<String>, Duckerror> {
        Err(unreachable_dispatch())
    }
}

impl files_host::Guest for Component {
    fn file_open(_url: String) -> Result<files_host::FileOpenResult, String> {
        Err("standalone build has no files backend".to_string())
    }
    fn file_read(_handle: u32, _offset: u64, _len: u32) -> Result<Vec<u8>, String> {
        Err("standalone build has no files backend".to_string())
    }
    fn file_close(_handle: u32) -> Result<(), String> {
        Err("standalone build has no files backend".to_string())
    }
}

impl table_stream_host::Guest for Component {
    fn filterable_table_list() -> Vec<table_stream_host::FilterableTable> {
        Vec::new()
    }
    fn ts_open_filtered(
        _handle: u32,
        _args: Vec<table_stream_host::Duckvalue>,
        _projection: Vec<u32>,
        _filters: Vec<table_stream_host::TsFilter>,
    ) -> Result<table_stream_host::TsOpenResult, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn ts_next(
        _handle: u32,
        _cursor: u32,
        _max_rows: u32,
    ) -> Result<table_stream_host::Resultset, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn ts_close(_handle: u32, _cursor: u32) -> Result<bool, Duckerror> {
        Err(unreachable_dispatch())
    }
}

// TVM spill tier. The standalone has no 64-bit host to own regions, so the stub
// declines region creation; the core's spill bridge (tvm_spill.rs) treats a
// failed create-region as "TVM unavailable", disables the bridge, and falls back
// to the ordinary temporary-directory spill path. The remaining entry points are
// then unreachable but must exist to satisfy the imports.
impl TvmManagerGuest for Component {
    fn create_region(_kind: RegionKind, _capacity: u32) -> Result<u16, TvmError> {
        Err(TvmError::AllocationFailed)
    }
    fn destroy_region(_region_id: u16) -> Result<(), TvmError> {
        Err(TvmError::AllocationFailed)
    }
    fn alloc(_region_id: u16, _size: u32) -> Result<Handle, TvmError> {
        Err(TvmError::AllocationFailed)
    }
    fn dealloc(_ptr: Handle) -> Result<(), TvmError> {
        Err(TvmError::AllocationFailed)
    }
    fn describe_region(_region_id: u16) -> Result<RegionInfo, TvmError> {
        Err(TvmError::AllocationFailed)
    }
}

impl TvmBytesGuest for Component {
    fn read(_ptr: Handle, _len: u32) -> Result<Vec<u8>, TvmError> {
        Err(TvmError::AllocationFailed)
    }
    fn write(_ptr: Handle, _data: Vec<u8>) -> Result<(), TvmError> {
        Err(TvmError::AllocationFailed)
    }
}

bindings::export!(Component with_types_in bindings);
