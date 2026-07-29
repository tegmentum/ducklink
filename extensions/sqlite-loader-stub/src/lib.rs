//! Declining stubs for the function-carrying host-side surfaces
//! sqlite-lib imports.
//!
//! Composed (via `wac plug`) into any standalone consumer after
//! `sqlite-lib.wasm` so its imports have a supplier at instantiation
//! time. None of the stubbed methods do real work — every call
//! returns the interface's most-declining reply (Err, `false`, an
//! empty payload). Runtime consumers that actually invoke one of
//! these methods will surface a clear "not supported in standalone"
//! error rather than silently getting a wrong answer.
//!
//! Interfaces stubbed here:
//!
//!   * `sqlite:wasm/extension-loader@0.1.0` — 3 methods (dynamic .load)
//!   * `sqlite:wasm/dispatch@0.1.0`         — 34 methods (scalar /
//!     aggregate / vtab / hook trampolines into the runtime extension
//!     registry the standalone build doesn't have)
//!   * `sqlite:wasm/opfs-host@0.1.0`        — 8 methods (browser OPFS
//!     VFS ops; native/wasi consumers reach the filesystem via wasi:fs
//!     rather than through opfs-host)
//!
//! `sqlite:extension/{types,http,policy,metadata,vtab}@1.0.0` are NOT
//! stubbed here — sqlite-lib imports them as pure type interfaces
//! (variants, records, no `func`s). `wac plug` only wires
//! function-carrying interfaces, so these stay as unresolved
//! type-only imports on the composed artifact; the ducklink host's
//! wasmtime component linker accepts them without a provider, the
//! same way the pre-Bug-9 sqlitewasm.wasm shipped at @0.1.0 with the
//! same type-only imports left dangling.

wit_bindgen::generate!({
    path: "wit",
    world: "sqlite:wasm-stub/sqlite-loader-stub",
    generate_all,
});

use exports::sqlite::wasm::dispatch::{
    Guest as DispatchGuest, IndexInfo, IndexPlan, VtabRow,
};
use exports::sqlite::wasm::extension_loader::{
    Guest as LoaderGuest, LoadOptions, LoaderError, Manifest,
};
use exports::sqlite::wasm::opfs_host::{
    Guest as OpfsGuest, OpfsError, OpfsErrorCode,
};

use sqlite::extension::types::{AuthAction, AuthResult, SqlValue, UpdateOperation};

const UNSUPPORTED_MSG: &str =
    "sqlite-loader-stub: not supported in standalone compose";

struct Stub;

// ─── extension-loader ────────────────────────────────────────────────
//
// Declining `.load` provider. Cache-component / sqlitewasm never call
// `load-extension`; the error path is exercised only by a future
// consumer that does.

fn loader_decline(op: &str) -> LoaderError {
    LoaderError {
        code: 1,
        message: format!(
            "sqlite-loader-stub: {op} is not supported (compose the real \
             loader if extension-loading is required)"
        ),
    }
}

impl LoaderGuest for Stub {
    fn load_extension(_path: String, _options: LoadOptions) -> Result<Manifest, LoaderError> {
        Err(loader_decline("load-extension"))
    }

    fn unload_extension(_name: String) -> Result<(), LoaderError> {
        Err(loader_decline("unload-extension"))
    }

    fn load_extension_from_uri(
        _uri: String,
        _options: LoadOptions,
    ) -> Result<Manifest, LoaderError> {
        Err(loader_decline("load-extension-from-uri"))
    }
}

// ─── dispatch ────────────────────────────────────────────────────────
//
// Trampolines the composed CLI uses to route scalar / aggregate / vtab
// / hook calls into a runtime-registered loaded extension. Standalone
// compose has no such registry — every call declines.
//
// Sensible defaults for plain (non-`result<>`) returns:
//
//   * `collation-compare` (s32) → 0. Zero means "equal"; SQLite's
//     sort still runs but rows keyed by this collation get merged.
//     Non-invocable in standalone anyway (no `.load`ed extension can
//     have registered a collation).
//
//   * `on-commit` (bool) → true. SQLite commit-hook contract:
//     non-zero converts the commit to a rollback. `true` is the
//     "let the commit through" answer.
//
//   * `wal-hook` (s32) → 0 (SQLITE_OK). Continues the outer SQL
//     statement without propagating an error.
//
//   * `authorize` → `AuthResult::Deny`. Fail-closed: an unroutable
//     authorizer means "refuse", not "allow".
//
//   * `vtab-eof` (bool) → true. Signals "no more rows"; a caller
//     stuck in an xNext loop terminates instead of hanging.
//
//   * `vtab-is-shadow-name` (bool) → false. Nothing is shadow-owned
//     by a stub-routed module.

fn dispatch_err(op: &str) -> String {
    format!("sqlite-loader-stub: dispatch::{op} is not supported in standalone compose")
}

impl DispatchGuest for Stub {
    fn scalar_call(
        _ext_name: String,
        _func_id: u64,
        _args: Vec<SqlValue>,
    ) -> Result<SqlValue, String> {
        Err(dispatch_err("scalar-call"))
    }

    fn aggregate_step(
        _ext_name: String,
        _func_id: u64,
        _context_id: u64,
        _args: Vec<SqlValue>,
    ) -> Result<(), String> {
        Err(dispatch_err("aggregate-step"))
    }

    fn aggregate_finalize(
        _ext_name: String,
        _func_id: u64,
        _context_id: u64,
    ) -> Result<SqlValue, String> {
        Err(dispatch_err("aggregate-finalize"))
    }

    fn aggregate_value(
        _ext_name: String,
        _func_id: u64,
        _context_id: u64,
    ) -> Result<SqlValue, String> {
        Err(dispatch_err("aggregate-value"))
    }

    fn aggregate_inverse(
        _ext_name: String,
        _func_id: u64,
        _context_id: u64,
        _args: Vec<SqlValue>,
    ) -> Result<(), String> {
        Err(dispatch_err("aggregate-inverse"))
    }

    fn collation_compare(
        _ext_name: String,
        _collation_id: u64,
        _a: String,
        _b: String,
    ) -> i32 {
        0
    }

    fn authorize(
        _ext_name: String,
        _action: AuthAction,
        _arg1: Option<String>,
        _arg2: Option<String>,
        _database: Option<String>,
        _trigger: Option<String>,
    ) -> AuthResult {
        AuthResult::Deny
    }

    fn on_update(
        _ext_name: String,
        _operation: UpdateOperation,
        _database: String,
        _table: String,
        _rowid: i64,
    ) {
        // No-op: no runtime hooks in standalone.
    }

    fn on_commit(_ext_name: String) -> bool {
        // Let the commit through; a rejected commit would look like the
        // stub is authoritative about transaction state, which it isn't.
        true
    }

    fn on_rollback(_ext_name: String) {
        // No-op: no runtime hooks in standalone.
    }

    fn wal_hook(
        _ext_name: String,
        _hook_id: u64,
        _db_name: String,
        _n_frames_in_wal: u32,
    ) -> i32 {
        0
    }

    fn vtab_create(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _db_name: String,
        _table_name: String,
        _args: Vec<String>,
    ) -> Result<String, String> {
        Err(dispatch_err("vtab-create"))
    }

    fn vtab_connect(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _db_name: String,
        _table_name: String,
        _args: Vec<String>,
    ) -> Result<String, String> {
        Err(dispatch_err("vtab-connect"))
    }

    fn vtab_destroy(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-destroy"))
    }

    fn vtab_disconnect(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-disconnect"))
    }

    fn vtab_best_index(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _info: IndexInfo,
    ) -> Result<IndexPlan, String> {
        Err(dispatch_err("vtab-best-index"))
    }

    fn vtab_open(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _cursor_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-open"))
    }

    fn vtab_close(
        _ext_name: String,
        _vtab_id: u64,
        _cursor_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-close"))
    }

    fn vtab_filter(
        _ext_name: String,
        _vtab_id: u64,
        _cursor_id: u64,
        _idx_num: i32,
        _idx_str: Option<String>,
        _args: Vec<SqlValue>,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-filter"))
    }

    fn vtab_next(
        _ext_name: String,
        _vtab_id: u64,
        _cursor_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-next"))
    }

    fn vtab_eof(_ext_name: String, _vtab_id: u64, _cursor_id: u64) -> bool {
        // "End of rows" — safer than false, which would loop forever.
        true
    }

    fn vtab_column(
        _ext_name: String,
        _vtab_id: u64,
        _cursor_id: u64,
        _col: i32,
    ) -> Result<SqlValue, String> {
        Err(dispatch_err("vtab-column"))
    }

    fn vtab_rowid(
        _ext_name: String,
        _vtab_id: u64,
        _cursor_id: u64,
    ) -> Result<i64, String> {
        Err(dispatch_err("vtab-rowid"))
    }

    fn vtab_fetch_batch(
        _ext_name: String,
        _vtab_id: u64,
        _cursor_id: u64,
        _max_rows: u32,
    ) -> Result<Vec<VtabRow>, String> {
        Err(dispatch_err("vtab-fetch-batch"))
    }

    fn vtab_update(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _args: Vec<SqlValue>,
    ) -> Result<i64, String> {
        Err(dispatch_err("vtab-update"))
    }

    fn vtab_begin(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-begin"))
    }

    fn vtab_sync(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-sync"))
    }

    fn vtab_commit(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-commit"))
    }

    fn vtab_rollback(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-rollback"))
    }

    fn vtab_rename(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _new_name: String,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-rename"))
    }

    fn vtab_savepoint(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _savepoint: i32,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-savepoint"))
    }

    fn vtab_release(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _savepoint: i32,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-release"))
    }

    fn vtab_rollback_to(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _savepoint: i32,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-rollback-to"))
    }

    fn vtab_is_shadow_name(_ext_name: String, _vtab_id: u64, _name: String) -> bool {
        false
    }

    fn vtab_integrity(
        _ext_name: String,
        _vtab_id: u64,
        _instance_id: u64,
        _schema: String,
        _table_name: String,
        _mode_flags: u32,
    ) -> Result<(), String> {
        Err(dispatch_err("vtab-integrity"))
    }
}

// ─── opfs-host ───────────────────────────────────────────────────────
//
// Host-side file ops backing the browser's OPFS VFS. In a
// standalone/native compose the wasi:filesystem preopens satisfy
// SQLite's I/O — opfs-host is never invoked. Every method returns
// `opfs-error{code=io, message=...}` so a caller who does reach the
// stub (misconfiguration, forced OPFS VFS on native) gets a clear
// IOERR rather than silent corruption.

fn opfs_err(op: &str) -> OpfsError {
    OpfsError {
        message: format!("sqlite-loader-stub: opfs-host::{op} — {UNSUPPORTED_MSG}"),
        code: OpfsErrorCode::Io,
    }
}

impl OpfsGuest for Stub {
    fn open(_path: String, _create: bool) -> Result<u64, OpfsError> {
        Err(opfs_err("open"))
    }

    fn read(_handle: u64, _offset: u64, _len: u32) -> Result<Vec<u8>, OpfsError> {
        Err(opfs_err("read"))
    }

    fn write(_handle: u64, _offset: u64, _data: Vec<u8>) -> Result<u32, OpfsError> {
        Err(opfs_err("write"))
    }

    fn truncate(_handle: u64, _size: u64) -> Result<(), OpfsError> {
        Err(opfs_err("truncate"))
    }

    fn sync(_handle: u64) -> Result<(), OpfsError> {
        Err(opfs_err("sync"))
    }

    fn size(_handle: u64) -> Result<u64, OpfsError> {
        Err(opfs_err("size"))
    }

    fn close(_handle: u64) -> Result<(), OpfsError> {
        Err(opfs_err("close"))
    }

    fn delete(_path: String) -> Result<(), OpfsError> {
        Err(opfs_err("delete"))
    }
}

export!(Stub);
