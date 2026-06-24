//! ducklink-runtime — the reusable wasm-component-loading engine.
//!
//! This crate is being extracted from `ducklink-host` so the same engine that
//! loads `duckdb:extension` wasm components can back two directions:
//!   1. the `ducklink` host, which runs DuckDB-compiled-to-wasm, and
//!   2. the native-DuckDB `ducklink` community extension (embeds wasmtime).
//!
//! Increment 1: the callback registry — maps a DuckDB-side function invocation
//! (by opaque handle) back to the owning wasm extension and its dispatcher.
//!
//! Increment 2: the `duckdb:extension` wasmtime bindings. The WIT world and its
//! generated host/guest types live here so both directions instantiate the same
//! component ABI; the host implements the `Host*` traits against its own store.
//!
//! Increment 4: the extension store-state + loaded-component instance (see the
//! [`extension`] module). The store-state implements the capability `Host*`
//! traits (capturing registrations into [`reg`]) and services config/logging
//! through an [`extension::ExtensionServices`] sink — the one direction-specific
//! seam.
use std::collections::HashMap;

pub mod extension;
pub use extension::{
    add_extension_interfaces_to_linker, describe_runtime_logicaltype, load_component,
    summarize_extopts, summarize_funcopts, summarize_registration_names, summarize_runtime_columns,
    summarize_runtime_funcargs, ConfigError, ExtensionInstance, ExtensionServices,
    ExtensionStoreState, LogField, LogLevel, PendingRegistrationsData,
};

/// The generated wasmtime bindings for the `duckdb:extension-host` world — the
/// capability surface a wasm extension component imports (register-scalar,
/// register-table, config, logging, catalog, files) plus the guest's exported
/// `load()` / `callback-dispatch`. Both the `ducklink` host and the native
/// `ducklink` DuckDB extension instantiate components against these bindings.
pub mod duckdb_extension_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension",
        require_store_data_send: true,
    });
}

/// Bindings for the storage-capable world (`duckdb-extension-storage`), which
/// additionally exports `storage-dispatch`. Only storage backend components
/// (e.g. sqlitewasm) satisfy this; the runtime builds these bindings lazily from
/// an already-loaded component instance so non-storage extensions (which don't
/// export storage-dispatch) still load against the base world above.
pub mod duckdb_extension_storage_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-storage",
        require_store_data_send: true,
    });
}

/// Bindings for the files-capable world (`duckdb-extension-files`), which
/// additionally exports `file-dispatch` (httpfs M2). Only files backend
/// components (e.g. webfs) satisfy this; the runtime builds these bindings
/// lazily from an already-loaded component instance so non-files extensions
/// (which don't export file-dispatch) still load against the base world above.
pub mod duckdb_extension_files_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-files",
        require_store_data_send: true,
    });
}

/// The kind of callback a handle dispatches to inside an extension component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackKind {
    Scalar,
    Table,
    Aggregate,
    Pragma,
    Cast,
}

impl CallbackKind {
    pub fn describe(self) -> &'static str {
        match self {
            CallbackKind::Scalar => "scalar",
            CallbackKind::Table => "table",
            CallbackKind::Aggregate => "aggregate",
            CallbackKind::Pragma => "pragma",
            CallbackKind::Cast => "cast",
        }
    }
}

/// Neutral registration model. A wasm extension's `load()` registers scalars,
/// tables, aggregates, macros, casts, etc. against the host's capability surface.
/// These types capture *what* was registered without referencing either the
/// wasm-DuckDB-core bindings (Direction 1) or the native DuckDB C API
/// (Direction 2), so the same capture path feeds both sinks. Each direction
/// converts these neutral records into its own loader/registration types.
pub mod reg {
    /// A DuckDB logical type, restricted to the value kinds the extension ABI
    /// currently exchanges.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum LogicalType {
        Boolean,
        Int64,
        Uint64,
        Float64,
        Text,
        Blob,
    }

    impl LogicalType {
        pub fn describe(self) -> &'static str {
            match self {
                LogicalType::Boolean => "BOOLEAN",
                LogicalType::Int64 => "INT64",
                LogicalType::Uint64 => "UINT64",
                LogicalType::Float64 => "FLOAT64",
                LogicalType::Text => "TEXT",
                LogicalType::Blob => "BLOB",
            }
        }
    }

    /// A scalar/aggregate/table function argument. `name` is optional because
    /// positional arguments may be anonymous.
    #[derive(Clone, Debug)]
    pub struct FuncArg {
        pub name: Option<String>,
        pub logical: LogicalType,
    }

    /// A named output column of a table function.
    #[derive(Clone, Debug)]
    pub struct ColumnDef {
        pub name: String,
        pub logical: LogicalType,
    }

    /// Function attribute flags (mirrors `duckdb:extension/types.funcflags`).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct FuncFlags {
        pub deterministic: bool,
        pub commutative: bool,
        pub stateless: bool,
        pub side_effecting: bool,
        pub deprecated: bool,
    }

    impl FuncFlags {
        pub fn describe(self) -> String {
            let mut parts = Vec::new();
            if self.deterministic {
                parts.push("deterministic");
            }
            if self.commutative {
                parts.push("commutative");
            }
            if self.stateless {
                parts.push("stateless");
            }
            if self.side_effecting {
                parts.push("sideeffecting");
            }
            if self.deprecated {
                parts.push("deprecated");
            }
            if parts.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", parts.join(", "))
            }
        }
    }

    /// Optional metadata attached to a scalar/aggregate registration.
    #[derive(Clone, Debug)]
    pub struct FuncOpts {
        pub description: Option<String>,
        pub tags: Vec<String>,
        pub attributes: FuncFlags,
    }

    /// Optional metadata attached to a table-function registration.
    #[derive(Clone, Debug)]
    pub struct ExtOpts {
        pub description: Option<String>,
        pub tags: Vec<String>,
    }

    /// A scalar value exchanged across the callback boundary.
    #[derive(Clone, Debug)]
    pub enum DuckValue {
        Null,
        Boolean(bool),
        Int64(i64),
        Uint64(u64),
        Float64(f64),
        Text(String),
        Blob(Vec<u8>),
    }

    /// A scalar function registered by an extension.
    #[derive(Clone, Debug)]
    pub struct ScalarReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub returns: LogicalType,
        pub callback_handle: u32,
        pub options: Option<FuncOpts>,
    }

    /// A table function registered by an extension.
    #[derive(Clone, Debug)]
    pub struct TableReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub columns: Vec<ColumnDef>,
        pub callback_handle: u32,
        pub options: Option<ExtOpts>,
    }

    /// A storage / catalog backend registered by an extension. Keyed by an
    /// ATTACH `type_name` (e.g. "sqlite"); `callback_handle` routes every
    /// `storage-dispatch` call back to the owning component.
    #[derive(Clone, Debug)]
    pub struct StorageReg {
        pub extension: String,
        pub type_name: String,
        pub callback_handle: u32,
        pub options: Option<ExtOpts>,
    }

    /// A files backend registered by an extension (httpfs M2). The
    /// `callback_handle` routes every `file-dispatch` call back to the owning
    /// component. Only one files backend is active at a time.
    #[derive(Clone, Debug)]
    pub struct FilesReg {
        pub extension: String,
        pub callback_handle: u32,
    }

    /// An aggregate function registered by an extension.
    #[derive(Clone, Debug)]
    pub struct AggregateReg {
        pub extension: String,
        pub name: String,
        pub arguments: Vec<FuncArg>,
        pub returns: LogicalType,
        pub callback_handle: u32,
        pub options: Option<FuncOpts>,
    }

    /// A SQL macro registered by an extension.
    #[derive(Clone, Debug)]
    pub struct MacroReg {
        pub extension: String,
        pub schema: String,
        pub name: String,
        pub parameters: Vec<String>,
        pub definition_sql: String,
    }

    /// A replacement scan binding a set of file extensions to a table function.
    #[derive(Clone, Debug)]
    pub struct ReplacementScanReg {
        pub extension: String,
        pub extensions: Vec<String>,
        pub function_name: String,
    }

    /// A user-defined logical type alias over a physical type.
    #[derive(Clone, Debug)]
    pub struct LogicalTypeReg {
        pub extension: String,
        pub name: String,
        pub physical: String,
    }

    /// A cast between two named types, dispatched through a callback.
    #[derive(Clone, Debug)]
    pub struct CastReg {
        pub extension: String,
        pub source: String,
        pub target: String,
        pub callback_handle: u32,
    }
}

/// One registered callback: which extension owns it, the guest-side dispatcher
/// handle to invoke, and the function kind.
#[derive(Clone, Debug)]
pub struct CallbackEntry {
    pub extension: String,
    pub dispatcher_handle: u32,
    pub kind: CallbackKind,
}

/// Allocates stable host-side handles and maps them to `CallbackEntry`s. The
/// host hands a handle to DuckDB at registration; DuckDB passes it back on every
/// invocation, and the engine routes it to the owning component.
#[derive(Default)]
pub struct CallbackRegistry {
    next_handle: u32,
    entries: HashMap<u32, CallbackEntry>,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        Self {
            next_handle: 1,
            entries: HashMap::new(),
        }
    }

    pub fn allocate(&mut self, extension: &str, kind: CallbackKind, dispatcher_handle: u32) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        self.entries.insert(
            handle,
            CallbackEntry {
                extension: extension.to_string(),
                dispatcher_handle,
                kind,
            },
        );
        eprintln!(
            "[extension-manager] registered {} callback handle {} for '{}' (dispatcher={dispatcher_handle})",
            kind.describe(),
            handle,
            extension
        );
        handle
    }

    pub fn remove(&mut self, handle: u32) {
        if let Some(entry) = self.entries.remove(&handle) {
            eprintln!(
                "[extension-manager] released {} callback handle {} for '{}'",
                entry.kind.describe(),
                handle,
                entry.extension
            );
        }
    }

    pub fn remove_extension(&mut self, extension: &str) {
        let initial = self.entries.len();
        self.entries.retain(|_, entry| entry.extension != extension);
        let removed = initial.saturating_sub(self.entries.len());
        if removed > 0 {
            eprintln!(
                "[extension-manager] purged {removed} callback handles after unloading '{}'",
                extension
            );
        }
    }

    pub fn get(&self, handle: u32) -> Option<CallbackEntry> {
        self.entries.get(&handle).cloned()
    }
}
