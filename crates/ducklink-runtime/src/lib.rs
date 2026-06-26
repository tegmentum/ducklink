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

use wasmtime::component::Component;
use wasmtime::Engine;

/// The MAJOR version of the `duckdb:extension` WIT contract this host speaks.
///
/// THE single edit point for a contract bump on the host side: bump this when the
/// canonical WIT package id moves to a new major (e.g. `duckdb:extension@3.0.0`).
/// The loader guard ([`check_component_contract`]) rejects any component whose
/// imported `duckdb:extension` package has a different major (or no version at
/// all -- a legacy, pre-versioning v1 component), so a mismatched component never
/// instantiates and silently marshals corrupted values.
pub const CONTRACT_MAJOR: u64 = 2;

/// Full contract version string the host advertises (observability only; the
/// guard compares the MAJOR via [`CONTRACT_MAJOR`]).
pub const CONTRACT_VERSION: &str = "2.0.0";

/// The host's `duckdb:extension` contract version, for logging / a built-in.
pub fn ducklink_contract_version() -> &'static str {
    CONTRACT_VERSION
}

/// The `duckdb:extension` contract major a component targets, read from its
/// imported package ids. Returns:
///   - `Some(major)` if it imports `duckdb:extension/...@MAJOR.minor.patch`
///   - `None` if it imports the package UNVERSIONED (legacy pre-versioning v1)
///
/// A component that imports nothing from `duckdb:extension` returns `None` too,
/// but in practice every loadable extension imports at least `runtime`/`types`.
pub fn component_contract_major(engine: &Engine, component: &Component) -> Option<u64> {
    for (name, _) in component.component_type().imports(engine) {
        // Import instance names look like `duckdb:extension/runtime@2.0.0` or,
        // for a legacy component, `duckdb:extension/runtime` (no version).
        let pkg = name.split('/').next().unwrap_or(name);
        if pkg.starts_with("duckdb:extension") {
            return match name.rsplit_once('@') {
                Some((_, ver)) => ver
                    .split('.')
                    .next()
                    .and_then(|m| m.parse::<u64>().ok()),
                None => None, // unversioned -> legacy v1
            };
        }
    }
    None
}

/// Loader pre-check: reject a component whose `duckdb:extension` contract major
/// differs from this host's [`CONTRACT_MAJOR`] (or is unversioned/legacy) with a
/// clear, actionable error BEFORE instantiation. Wasmtime would itself reject a
/// truly mismatched component at instantiate time, but with a cryptic
/// type-mismatch trap; this gives the friendly message and explicitly catches the
/// unversioned-legacy case (which can silently marshal corrupted values because
/// the rich-types bump shifted enum discriminants).
pub fn check_component_contract(
    engine: &Engine,
    component: &Component,
    extension_name: &str,
) -> wasmtime::Result<()> {
    match component_contract_major(engine, component) {
        Some(major) if major == CONTRACT_MAJOR => Ok(()),
        Some(major) => Err(wasmtime::Error::msg(format!(
            "component '{extension_name}' targets duckdb:extension contract {major}.x \
             but this ducklink speaks contract {CONTRACT_MAJOR}.x; rebuild the component \
             against the current WIT (or use the matching ducklink version)"
        ))),
        None => Err(wasmtime::Error::msg(format!(
            "component '{extension_name}' targets an UNVERSIONED duckdb:extension contract \
             (legacy v1) but this ducklink speaks contract {CONTRACT_MAJOR}.x; rebuild the \
             component against the current WIT (or use the matching ducklink version)"
        ))),
    }
}

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

/// Bindings for the index-capable world (`duckdb-extension-index`), which
/// additionally exports `index-dispatch` (Item 3 / M2a custom index). Only
/// custom-index backend components (e.g. hnswfns) satisfy this; the runtime
/// builds these bindings lazily from an already-loaded component instance so
/// non-index extensions (which don't export index-dispatch) still load against
/// the base world above.
pub mod duckdb_extension_index_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension-index",
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
    /// currently exchanges. NOTE: no longer `Copy` -- the `Complex` escape-hatch
    /// arm carries an owned type-expression `String`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum LogicalType {
        Boolean,
        Int64,
        Uint64,
        Float64,
        Text,
        Blob,
        Int32,
        Timestamp,
        Int8,
        Int16,
        Uint8,
        Uint16,
        Uint32,
        Float32,
        Date,
        Time,
        Timestamptz,
        Decimal,
        Interval,
        Uuid,
        /// ESCAPE-HATCH: a DuckDB type-expression string (e.g. "INTEGER[]",
        /// "STRUCT(a INTEGER, b VARCHAR)"). Carries an arbitrary declared return
        /// type the core resolves at registration time.
        Complex(String),
    }

    impl LogicalType {
        pub fn describe(&self) -> String {
            match self {
                LogicalType::Boolean => "BOOLEAN".to_string(),
                LogicalType::Int64 => "INT64".to_string(),
                LogicalType::Uint64 => "UINT64".to_string(),
                LogicalType::Float64 => "FLOAT64".to_string(),
                LogicalType::Text => "TEXT".to_string(),
                LogicalType::Blob => "BLOB".to_string(),
                LogicalType::Int32 => "INT32".to_string(),
                LogicalType::Timestamp => "TIMESTAMP".to_string(),
                LogicalType::Int8 => "INT8".to_string(),
                LogicalType::Int16 => "INT16".to_string(),
                LogicalType::Uint8 => "UINT8".to_string(),
                LogicalType::Uint16 => "UINT16".to_string(),
                LogicalType::Uint32 => "UINT32".to_string(),
                LogicalType::Float32 => "FLOAT32".to_string(),
                LogicalType::Date => "DATE".to_string(),
                LogicalType::Time => "TIME".to_string(),
                LogicalType::Timestamptz => "TIMESTAMPTZ".to_string(),
                LogicalType::Decimal => "DECIMAL".to_string(),
                LogicalType::Interval => "INTERVAL".to_string(),
                LogicalType::Uuid => "UUID".to_string(),
                LogicalType::Complex(expr) => expr.clone(),
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
        Int32(i32),
        /// Microseconds since 1970-01-01 (DuckDB's TIMESTAMP representation).
        Timestamp(i64),
        Int8(i8),
        Int16(i16),
        Uint8(u8),
        Uint16(u16),
        Uint32(u32),
        Float32(f32),
        /// Days since 1970-01-01 (DuckDB's DATE representation).
        Date(i32),
        /// Microseconds since midnight (DuckDB's TIME representation).
        Time(i64),
        /// Microseconds since 1970-01-01 UTC (DuckDB's TIMESTAMP_TZ representation).
        Timestamptz(i64),
        /// HUGEINT-backed scaled decimal: value = (upper<<64 | lower), with
        /// `width` total digits and `scale` fractional digits.
        Decimal {
            lower: u64,
            upper: u64,
            width: u8,
            scale: u8,
        },
        /// INTERVAL: months + days + microseconds.
        Interval {
            months: i32,
            days: i32,
            micros: i64,
        },
        /// 128-bit UUID logical value, split into hi/lo halves.
        Uuid {
            hi: u64,
            lo: u64,
        },
        /// ESCAPE-HATCH composite value: a DuckDB type-expression string plus the
        /// value rendered as JSON. The core reconstructs the real LIST/STRUCT vector
        /// from the JSON via the duckdb C vector API.
        Complex {
            type_expr: String,
            json: String,
        },
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

    /// A custom index TYPE registered by an extension (Item 3 / M2a). Declares an
    /// index type `type_name` (e.g. "wasm_hnsw") so `CREATE INDEX ... USING
    /// <type_name>` routes the build pipeline to the owning component's
    /// `index-dispatch` export. The build/search lifecycle is keyed by index NAME
    /// inside the component, so no per-type callback handle is needed.
    #[derive(Clone, Debug)]
    pub struct IndexReg {
        pub extension: String,
        pub type_name: String,
    }

    /// A collation registered by an extension (Item 2). Declares a collation
    /// `name` whose transform is the already-registered scalar `transform_scalar`
    /// (text -> sort-key text). The collation reuses the existing scalar dispatch
    /// entirely, so no callback handle is needed: the core looks the scalar up by
    /// name in the catalog when it registers the collation.
    #[derive(Clone, Debug)]
    pub struct CollationReg {
        pub extension: String,
        pub name: String,
        pub transform_scalar: String,
        pub combinable: bool,
    }

    /// A PRAGMA declared by an extension (Item 4). The user types
    /// `PRAGMA <name>(...)`; the core intercepts it, dispatches via the
    /// `callback_handle` (callback-dispatch.call-pragma), and the component
    /// RETURNS a SQL script (text) that the core then runs on the connection.
    /// No mid-callback re-entry into SQL, so no connection re-entrancy.
    #[derive(Clone, Debug)]
    pub struct PragmaReg {
        pub extension: String,
        pub name: String,
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
