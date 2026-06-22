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
use std::collections::HashMap;

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
