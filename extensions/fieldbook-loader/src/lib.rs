//! Fieldbook loader stub for the standalone (wac-composed) `fieldbook-cli.wasm`
//! deployment. Structurally identical to `crates/ducklink-loader` (declining
//! defaults for callback-dispatch and the eight host->core backend surfaces
//! plus tvm:memory) with one tweak: `request-load` returns `true` for
//! `"fieldbook"` and `"fieldbook_dotcmd"` so the CLI's `LOAD fieldbook;`
//! autoload path reports success even though no dynamic extension actually
//! runs — the fieldbook authoring surface (`.fb`, `.entry`, `.run`) reaches
//! the user via the wac-plugged fieldbook-dotcmd component. Nothing ever
//! registers a callback handle in this compose, so the dispatch and backend
//! entry points are unreachable in practice but must exist to type-check.
#[allow(warnings)]
mod bindings;

use bindings::exports::duckdb::cli::dotcmd_host::{CommandInfo, Guest as DotcmdHostGuest, Outcome};
use bindings::exports::duckdb::component::extension_loader_hooks::{
    Guest as HooksGuest, PendingRegistrations,
};
use bindings::exports::duckdb::component::host_extension_loader::Guest as LoaderGuest;
use bindings::exports::duckdb::extension::callback_dispatch::{
    Colvec, Duckerror, Duckvalue, Guest as DispatchGuest, Invokeinfo, Resultset,
};
use bindings::exports::duckdb::extension::{
    collation_host, files_host, index_host, optimizer_host, parser_host, pragma_host, storage_host,
    table_stream_host,
};
use bindings::exports::tvm::memory::bytes::Guest as TvmBytesGuest;
use bindings::exports::tvm::memory::manager::{
    Guest as TvmManagerGuest, Handle, RegionInfo, RegionKind, TvmError,
};

struct Component;

impl LoaderGuest for Component {
    fn request_load(name: String) -> bool {
        // The fieldbook standalone bakes in `fieldbook_dotcmd.wasm` (dotcmd
        // authoring surface) via wac plug. `fieldbook.wasm` (engine scalars) is
        // NOT plugged today — it targets duckdb:extension@5.0.0 while the core
        // is still @4.0.0 (see docs/fieldbook-wasm-phase0-findings.md §2.3 and
        // §4.1: the shared nested-exec/@5 ecosystem rebuild). We still answer
        // `true` for both names so a user typing `LOAD fieldbook;` at the REPL
        // sees "success" rather than a load-declined error; the dot-command
        // surface works independent of that.
        matches!(name.as_str(), "fieldbook" | "fieldbook_dotcmd")
    }
}

impl HooksGuest for Component {
    fn get_pending_registrations() -> PendingRegistrations {
        // Nothing to register — no in-guest extensions live inside this stub.
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
    Duckerror::Internal("fieldbook standalone has no loadable extension callbacks".to_string())
}

impl DispatchGuest for Component {
    fn call_scalar(
        _handle: u32,
        _args: Vec<Duckvalue>,
        _ctx: Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }

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

    fn call_pragma(_handle: u32, _args: Vec<Duckvalue>) -> Result<Option<Duckvalue>, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_cast(_handle: u32, _value: Duckvalue) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }
}

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
    fn storage_scan_next(_scan: u32, _max_rows: u32) -> Result<storage_host::Resultset, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_scan_close(_scan: u32) -> Result<bool, Duckerror> {
        Err(unreachable_dispatch())
    }
    // Write-side surface (added when the storage backend gained
    // INSERT/UPDATE/DELETE/CREATE TABLE support). All routes decline in the
    // standalone stub — nothing ever hands out a catalog handle to write on.
    fn storage_begin_transaction(_catalog: u32) -> Result<u32, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_commit_transaction(_txn: u32) -> Result<(), Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_rollback_transaction(_txn: u32) -> Result<(), Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_create_table(
        _txn: u32,
        _table: String,
        _columns: Vec<storage_host::Columndef>,
    ) -> Result<(), Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_insert_rows(
        _txn: u32,
        _table: String,
        _rows: Vec<Vec<storage_host::Duckvalue>>,
    ) -> Result<u64, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_delete_rows(_txn: u32, _table: String, _rowids: Vec<i64>) -> Result<u64, Duckerror> {
        Err(unreachable_dispatch())
    }
    fn storage_update_rows(
        _txn: u32,
        _table: String,
        _rowids: Vec<i64>,
        _updated_columns: Vec<u32>,
        _rows: Vec<Vec<storage_host::Duckvalue>>,
    ) -> Result<u64, Duckerror> {
        Err(unreachable_dispatch())
    }
}

impl index_host::Guest for Component {
    fn index_type_list() -> Vec<String> {
        Vec::new()
    }
    fn index_create(_type_name: String, _index_name: String, _dims: u32) -> Result<u32, Duckerror> {
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
        Err("fieldbook standalone has no files backend".to_string())
    }
    fn file_read(_handle: u32, _offset: u64, _len: u32) -> Result<Vec<u8>, String> {
        Err("fieldbook standalone has no files backend".to_string())
    }
    fn file_close(_handle: u32) -> Result<(), String> {
        Err("fieldbook standalone has no files backend".to_string())
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

// No-op dotcmd-host stub. In the native runtime this is served by the
// pluggable-dotcmd dispatcher in `ducklink-host`; in the standalone wac compose
// there's no such dispatcher, so `invoke` reports "no such dot command" (the
// CLI then falls back to its compiled-in built-ins like `.help`, `.quit`,
// `.tables`) and `list-commands` returns an empty list. See
// docs/fieldbook-wasm-phase0-findings.md §4.2 for why standalone dotcmd wiring
// is deferred (the fieldbook-dotcmd component uses a different `duckdb:dotcmd`
// world that today only the native host bridges).
impl DotcmdHostGuest for Component {
    fn invoke(_name: String, _args: String) -> Result<Option<Outcome>, String> {
        Ok(None)
    }
    fn list_commands() -> Vec<CommandInfo> {
        Vec::new()
    }
}

bindings::export!(Component with_types_in bindings);
