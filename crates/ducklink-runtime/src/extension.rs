//! The reusable extension store-state + loaded-component instance.
//!
//! `ExtensionStoreState` implements the `duckdb:extension` host capability
//! traits: it captures what a component's `load()` registers (into the neutral
//! [`crate::reg`] model) and services the component's config/logging requests
//! through an [`ExtensionServices`] sink. The sink is the one direction-specific
//! seam — the `ducklink` host routes it to DuckDB-compiled-to-wasm; the native
//! `ducklink` extension will route it to native DuckDB.
//!
//! `ExtensionInstance` is a loaded component: its `Store<ExtensionStoreState>`
//! plus generated bindings, with `dispatch_*` re-entering the guest's
//! `callback-dispatch` export for each DuckDB-side invocation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use wasmtime::component::{Component, Linker, ResourceTable};
#[cfg(test)]
use wasmtime::component::Resource;
use wasmtime::{AsContextMut, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

// Phase 6.2.o — the wit-bindgen wrapper `duckdb_extension_bindings`
// retired entirely (Phase 6.2.n Sessions 3-5 migrated every
// test-only inherent method signature off wit-bindgen record args;
// the remaining 60 From converter blocks + convert_extension_*
// helpers cascade-retired here). Production + test both name only
// the Phase 6.2.m mirrors on `crate::extension`.
// Phase 6.2.j — DuckdbExtension typed dispatcher retired (used to be
// constructed at load time; every dispatch now flows through the
// wasmos sync export bridge with the raw Instance). DuckdbExtensionPre
// was unused in this crate already. The interface types (extension_
// types, extension_column_types, etc.) are imported above from the
// same bindings module.
// use crate::duckdb_extension_bindings::{DuckdbExtension, DuckdbExtensionPre};
use crate::reg;
use crate::{CallbackKind, CallbackRegistry};

#[cfg(test)]
type BindgenVec<T> = wasmtime::component::__internal::Vec<T>;

// ---------------------------------------------------------------------------
// major-4 columnar adaptation (native host)
// ---------------------------------------------------------------------------
//
// The major-4 dispatch ABI is columnar (`call-scalar-batch-col` /
// `call-aggregate-col` / `call-cast-col` take/return `colvec`s). The native
// host bridge still assembles row-major `Duckvalue`s from native DuckDB
// vectors, so these helpers pivot row-major <-> columnar AT the wasmtime
// boundary. This keeps the (large) native DuckDB-vector reading path unchanged
// while speaking the columnar contract; the bulk-memcpy win is realized in the
// wasm core (which reads DuckDB vectors directly into colvecs). Correctness +
// NULL handling are identical to the row-major path.

/// Build one columnar `colvec` from a column of row-major `Duckvalue`s. The arm
/// is chosen from the first non-NULL value (a component column is homogeneous);
/// NULLs become a typed placeholder plus a cleared validity bit.
fn column_from_values(vals: &[&Duckvalue]) -> Colvec {
    use Column;
    use Duckvalue as D;
    let n = vals.len();
    let mut validity: Vec<u8> = Vec::new();
    let mark_null = |row: usize, validity: &mut Vec<u8>| {
        if validity.is_empty() {
            *validity = vec![0xFFu8; (n + 7) / 8];
        }
        validity[row >> 3] &= !(1u8 << (row & 7));
    };
    // Representative non-null value picks the column arm.
    let rep = vals.iter().find(|v| !matches!(v, D::Null));
    macro_rules! build {
        ($arm:ident, $default:expr, $pat:pat => $extract:expr) => {{
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    $pat => out.push($extract),
                    _ => {
                        mark_null(r, &mut validity);
                        out.push($default);
                    }
                }
            }
            Column::$arm(out)
        }};
    }
    let data = match rep {
        None => {
            // all-NULL (or empty) column: emit an all-null int64 column.
            for r in 0..n {
                mark_null(r, &mut validity);
            }
            Column::Int64(vec![0i64; n])
        }
        Some(D::Boolean(_)) => build!(Boolean, false, D::Boolean(x) => *x),
        Some(D::Int64(_)) => build!(Int64, 0i64, D::Int64(x) => *x),
        Some(D::Uint64(_)) => build!(Uint64, 0u64, D::Uint64(x) => *x),
        Some(D::Float64(_)) => build!(Float64, 0.0f64, D::Float64(x) => *x),
        Some(D::Int32(_)) => build!(Int32, 0i32, D::Int32(x) => *x),
        Some(D::Int16(_)) => build!(Int16, 0i16, D::Int16(x) => *x),
        Some(D::Int8(_)) => build!(Int8, 0i8, D::Int8(x) => *x),
        Some(D::Uint32(_)) => build!(Uint32, 0u32, D::Uint32(x) => *x),
        Some(D::Uint16(_)) => build!(Uint16, 0u16, D::Uint16(x) => *x),
        Some(D::Uint8(_)) => build!(Uint8, 0u8, D::Uint8(x) => *x),
        Some(D::Float32(_)) => build!(Float32, 0.0f32, D::Float32(x) => *x),
        Some(D::Timestamp(_)) => build!(Timestamp, 0i64, D::Timestamp(x) => *x),
        Some(D::Time(_)) => build!(Time, 0i64, D::Time(x) => *x),
        Some(D::Timestamptz(_)) => build!(Timestamptz, 0i64, D::Timestamptz(x) => *x),
        Some(D::Date(_)) => build!(Date, 0i32, D::Date(x) => *x),
        Some(D::Text(_)) => build!(Text, String::new(), D::Text(x) => x.clone()),
        Some(D::Blob(_)) => build!(Blob, Vec::new(), D::Blob(x) => x.clone()),
        Some(D::Decimal(_)) => {
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    D::Decimal(d) => out.push(Decimalvalue {
                        lower: d.lower,
                        upper: d.upper,
                        width: d.width,
                        scale: d.scale,
                    }),
                    _ => {
                        mark_null(r, &mut validity);
                        out.push(Decimalvalue {
                            lower: 0,
                            upper: 0,
                            width: 0,
                            scale: 0,
                        });
                    }
                }
            }
            Column::Decimal(out)
        }
        Some(D::Interval(_)) => {
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    D::Interval(d) => out.push(Intervalvalue {
                        months: d.months,
                        days: d.days,
                        micros: d.micros,
                    }),
                    _ => {
                        mark_null(r, &mut validity);
                        out.push(Intervalvalue {
                            months: 0,
                            days: 0,
                            micros: 0,
                        });
                    }
                }
            }
            Column::Interval(out)
        }
        Some(D::Uuid(_)) => {
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    D::Uuid(d) => {
                        out.push(Uuidvalue { hi: d.hi, lo: d.lo })
                    }
                    _ => {
                        mark_null(r, &mut validity);
                        out.push(Uuidvalue { hi: 0, lo: 0 });
                    }
                }
            }
            Column::Uuid(out)
        }
        // T2-1 residual (major-5): pivot HUGEINT / UHUGEINT scalars into the
        // fixed-width column arms carrying two u64/s64 halves.
        Some(D::Hugeint(_)) => {
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    D::Hugeint(h) => out.push(DuckInt128 {
                        lower: h.lower,
                        upper: h.upper,
                    }),
                    _ => {
                        mark_null(r, &mut validity);
                        out.push(DuckInt128 { lower: 0, upper: 0 });
                    }
                }
            }
            Column::Hugeint(out)
        }
        Some(D::Uhugeint(_)) => {
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    D::Uhugeint(h) => out.push(DuckUint128 {
                        lower: h.lower,
                        upper: h.upper,
                    }),
                    _ => {
                        mark_null(r, &mut validity);
                        out.push(DuckUint128 { lower: 0, upper: 0 });
                    }
                }
            }
            Column::Uhugeint(out)
        }
        Some(D::Complex(_)) => {
            let mut out = Vec::with_capacity(n);
            for (r, v) in vals.iter().enumerate() {
                match v {
                    D::Complex(c) => out.push(Complexvalue {
                        type_expr: c.type_expr.clone(),
                        json: c.json.clone(),
                    }),
                    _ => {
                        mark_null(r, &mut validity);
                        out.push(Complexvalue {
                            type_expr: String::new(),
                            json: "null".into(),
                        });
                    }
                }
            }
            Column::Complex(out)
        }
        Some(D::Null) => unreachable!("rep is a non-null value"),
    };
    Colvec {
        data,
        validity,
        rows: n as u32,
    }
}

/// Pivot a row-major batch to one `colvec` per argument column.
fn rows_to_colvecs(
    rows: &[Vec<Duckvalue>],
) -> Vec<Colvec> {
    let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
    (0..ncols)
        .map(|j| {
            let col: Vec<&Duckvalue> = rows.iter().map(|r| &r[j]).collect();
            column_from_values(&col)
        })
        .collect()
}

/// Lower a result `colvec` back to a row-major `Vec<Duckvalue>` (validity =>
/// `Null`). The inverse of [`column_from_values`].
fn colvec_to_values(c: Colvec) -> Vec<Duckvalue> {
    use Column;
    use Duckvalue as D;
    let n = c.rows as usize;
    let is_valid = |i: usize| -> bool {
        c.validity.is_empty()
            || (i >> 3 >= c.validity.len())
            || (c.validity[i >> 3] >> (i & 7)) & 1 != 0
    };
    let mut out: Vec<D> = Vec::with_capacity(n);
    macro_rules! emit {
        ($v:expr, $ctor:expr) => {{
            for (i, x) in $v.into_iter().enumerate() {
                out.push(if is_valid(i) { $ctor(x) } else { D::Null });
            }
        }};
    }
    match c.data {
        Column::Boolean(v) => emit!(v, D::Boolean),
        Column::Int64(v) => emit!(v, D::Int64),
        Column::Uint64(v) => emit!(v, D::Uint64),
        Column::Float64(v) => emit!(v, D::Float64),
        Column::Int32(v) => emit!(v, D::Int32),
        Column::Int16(v) => emit!(v, D::Int16),
        Column::Int8(v) => emit!(v, D::Int8),
        Column::Uint32(v) => emit!(v, D::Uint32),
        Column::Uint16(v) => emit!(v, D::Uint16),
        Column::Uint8(v) => emit!(v, D::Uint8),
        Column::Float32(v) => emit!(v, D::Float32),
        Column::Timestamp(v) => emit!(v, D::Timestamp),
        Column::Time(v) => emit!(v, D::Time),
        Column::Timestamptz(v) => emit!(v, D::Timestamptz),
        Column::Date(v) => emit!(v, D::Date),
        Column::Text(v) => emit!(v, D::Text),
        Column::Blob(v) => emit!(v, D::Blob),
        Column::Decimal(v) => {
            for (i, d) in v.into_iter().enumerate() {
                out.push(if is_valid(i) {
                    D::Decimal(Decimalvalue {
                        lower: d.lower,
                        upper: d.upper,
                        width: d.width,
                        scale: d.scale,
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::Interval(v) => {
            for (i, d) in v.into_iter().enumerate() {
                out.push(if is_valid(i) {
                    D::Interval(Intervalvalue {
                        months: d.months,
                        days: d.days,
                        micros: d.micros,
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::Uuid(v) => {
            for (i, d) in v.into_iter().enumerate() {
                out.push(if is_valid(i) {
                    D::Uuid(Uuidvalue { hi: d.hi, lo: d.lo })
                } else {
                    D::Null
                });
            }
        }
        // T2-1 residual (major-5): 128-bit integer columns lift back to the
        // row-major HUGEINT / UHUGEINT arms carrying two u64/s64 halves.
        Column::Hugeint(v) => {
            for (i, h) in v.into_iter().enumerate() {
                out.push(if is_valid(i) {
                    D::Hugeint(Hugeintvalue {
                        lower: h.lower,
                        upper: h.upper,
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::Uhugeint(v) => {
            for (i, h) in v.into_iter().enumerate() {
                out.push(if is_valid(i) {
                    D::Uhugeint(Uhugeintvalue {
                        lower: h.lower,
                        upper: h.upper,
                    })
                } else {
                    D::Null
                });
            }
        }
        // S1 (major-5): nested-column arms carry an opaque byte payload
        // (`nested-column { encoded: list<u8> }` / `map-column` /
        // `array-column`). The row-major `Duckvalue` has no first-class
        // LIST/STRUCT/MAP/ARRAY arm (see types.wit), so we degrade to
        // `Duckvalue::Complex` -- one row per column slot, all sharing the
        // same encoded blob (the runtime-defined per-vector encoding is
        // outside this scope; a future @6 will replace this with a
        // structural nested-VALUE crossing once wit-parser gains recursive-
        // value-type support). The type-expression tag records the KIND so
        // downstream can dispatch.
        Column::ListCol(nc) => {
            let json = nested_column_json(&nc.encoded);
            for i in 0..n {
                out.push(if is_valid(i) {
                    D::Complex(Complexvalue {
                        type_expr: "LIST".into(),
                        json: json.clone(),
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::StructCol(nc) => {
            let json = nested_column_json(&nc.encoded);
            for i in 0..n {
                out.push(if is_valid(i) {
                    D::Complex(Complexvalue {
                        type_expr: "STRUCT".into(),
                        json: json.clone(),
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::MapCol(mc) => {
            let json = format!(
                "{{\"keys\":{},\"vals\":{}}}",
                nested_column_json(&mc.keys_encoded),
                nested_column_json(&mc.vals_encoded),
            );
            for i in 0..n {
                out.push(if is_valid(i) {
                    D::Complex(Complexvalue {
                        type_expr: "MAP".into(),
                        json: json.clone(),
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::ArrayCol(ac) => {
            let json = format!(
                "{{\"size\":{},\"encoded\":{}}}",
                ac.size,
                nested_column_json(&ac.encoded),
            );
            for i in 0..n {
                out.push(if is_valid(i) {
                    D::Complex(Complexvalue {
                        type_expr: "ARRAY".into(),
                        json: json.clone(),
                    })
                } else {
                    D::Null
                });
            }
        }
        Column::Complex(v) => {
            for (i, c) in v.into_iter().enumerate() {
                out.push(if is_valid(i) {
                    D::Complex(Complexvalue {
                        type_expr: c.type_expr,
                        json: c.json,
                    })
                } else {
                    D::Null
                });
            }
        }
    }
    out
}

/// Render a nested-column opaque byte payload as an escaped JSON string --
/// stub for the S1 nested-column arms in the row-major `colvec_to_values`
/// fallback path. Callers that need to actually decode the payload go through
/// the runtime-defined encoding (out of scope for this phase).
fn nested_column_json(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2 + 2);
    s.push('"');
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('"');
    s
}

// ---------------------------------------------------------------------------
// Service sink (the one direction-specific seam)
// ---------------------------------------------------------------------------

/// A configuration error surfaced to a component. Neutral mirror of
/// `duckdb:extension/types.config-error`.
#[derive(Debug, Clone)]
pub enum ConfigError {
    InvalidKey(String),
    TypeMismatch(String),
    Unavailable(String),
    InternalConfig(String),
}

/// A log severity. Neutral mirror of `duckdb:extension/logging.log-level`.
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// A structured log field (key/value). Neutral mirror of
/// `duckdb:extension/logging.log-field`.
#[derive(Debug, Clone)]
pub struct LogField {
    pub key: String,
    pub value: String,
}

/// Result of a `nested-exec` invocation. Neutral mirror of
/// `duckdb:extension/nested-exec.exec-result`. Either `rows` (for row-producing
/// statements) or `rows_affected` (for DML) is populated; both may be `None`
/// for a statement that produced neither (e.g. `SET`, `PRAGMA` with no result).
#[derive(Debug, Clone, Default)]
pub struct NestedExecResult {
    pub rows: Option<Vec<Vec<String>>>,
    pub rows_affected: Option<u64>,
}

/// Maximum nesting depth the host enforces for `nested-exec`. Applied per
/// OS-thread (a re-entrant chain of extension callbacks stays on one thread,
/// so a thread-local counter catches accidental cascades — e.g. a fieldbook
/// entry that itself calls `fieldbook_run`). Documented default from the WIT.
pub const NESTED_EXEC_MAX_DEPTH: u32 = 4;

thread_local! {
    /// Per-OS-thread nesting-depth counter for `nested-exec`. Bumped on entry
    /// via [`NestedExecDepthGuard::enter`], decremented on drop. `Cell<u32>`
    /// is single-threaded by construction; every increment/decrement runs on
    /// the same OS thread by design (extension callbacks are synchronous and
    /// don't hand off).
    static NESTED_EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII guard that bumps the per-thread nested-exec depth on construction and
/// decrements it on drop. `enter` returns `Err(message)` when a bump would push
/// the counter past [`NESTED_EXEC_MAX_DEPTH`] — the counter is left unchanged
/// in that case, so the caller must not decrement it.
#[cfg(test)]
struct NestedExecDepthGuard;

#[cfg(test)]
impl NestedExecDepthGuard {
    fn enter() -> Result<Self, String> {
        NESTED_EXEC_DEPTH.with(|d| {
            let cur = d.get();
            if cur >= NESTED_EXEC_MAX_DEPTH {
                return Err(format!(
                    "nested-exec: max nesting depth {NESTED_EXEC_MAX_DEPTH} exceeded"
                ));
            }
            d.set(cur + 1);
            Ok(Self)
        })
    }
}

#[cfg(test)]
impl Drop for NestedExecDepthGuard {
    fn drop(&mut self) {
        NESTED_EXEC_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Services a loaded component requests from the running database: reading
/// configuration and emitting logs. Implemented per direction (the host routes
/// to DuckDB-compiled-to-wasm; the native extension to native DuckDB).
///
/// `Send` so `ExtensionStoreState` can move across the loader thread.
pub trait ExtensionServices: Send {
    fn provider_version(&mut self) -> Result<String, ConfigError>;
    fn list_keys(&mut self, prefix: Option<&str>) -> Result<Vec<String>, ConfigError>;
    fn get_string(&mut self, path: &str) -> Result<Option<String>, ConfigError>;
    fn get_bool(&mut self, path: &str) -> Result<Option<bool>, ConfigError>;
    fn get_i64(&mut self, path: &str) -> Result<Option<i64>, ConfigError>;
    fn get_u64(&mut self, path: &str) -> Result<Option<u64>, ConfigError>;
    fn get_f64(&mut self, path: &str) -> Result<Option<f64>, ConfigError>;
    fn get_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, ConfigError>;
    fn get_string_list(&mut self, path: &str) -> Result<Option<Vec<String>>, ConfigError>;
    fn log(&mut self, level: LogLevel, message: &str, target: Option<&str>);
    fn log_fields(&mut self, level: LogLevel, message: &str, fields: &[LogField]);

    /// v1.1 live-query host import (catalog completion). Run `sql` (a read-only
    /// SELECT) on the live database and return the rows as text cells (every cell
    /// stringified; NULL -> ""). BEST-EFFORT: if the core is busy (the call
    /// arrives from inside a query callback, so the executor is already locked /
    /// mid-call) or the SQL fails, return Err(message) and the caller degrades.
    /// The default impl reports unavailability, so directions that don't wire a
    /// live connection (e.g. tests) still compile.
    fn query(&mut self, _sql: &str) -> Result<Vec<Vec<String>>, String> {
        Err("live query not available in this host".to_string())
    }

    /// EXECUTE-capable counterpart to [`query`](Self::query). Run `sql` on a
    /// SIBLING connection to the same database — a fresh, autocommitted
    /// transaction that sidesteps the outer statement's core-mutex + wasm-store
    /// re-entrancy. Callable from inside a scalar/table callback.
    ///
    /// Directions implement this with their own concurrency-safe path:
    /// native-DuckDB opens a fresh `duckdb_connect` on the shared db handle;
    /// the wasm-core host cannot safely re-enter its single core store and
    /// therefore returns an error today (a future minor may lift this).
    ///
    /// The default returns Err so directions without exec plumbing still compile.
    fn nested_exec(&mut self, _sql: &str) -> Result<NestedExecResult, String> {
        Err("nested-exec not available in this host".to_string())
    }
}

// Phase 6.2.l.2 — `neutral_configerror_to_ext` and
// `ext_loglevel_to_neutral` retired here. They were used only by
// the deleted extension_config::Host and extension_logging::Host
// impls; the wasmos-native ConfigHost / LoggingHost in
// extension_wasmos.rs have their own local converters.

// ---------------------------------------------------------------------------
// Pending-registration buffers
// ---------------------------------------------------------------------------

type PendingScalar = reg::ScalarReg;
type PendingTable = reg::TableReg;
type PendingAggregate = reg::AggregateReg;
type PendingMacro = reg::MacroReg;
type PendingReplacementScan = reg::ReplacementScanReg;
type PendingLogicalType = reg::LogicalTypeReg;
type PendingCast = reg::CastReg;
type PendingStorage = reg::StorageReg;
type PendingIndex = reg::IndexReg;
type PendingFiles = reg::FilesReg;
type PendingCollation = reg::CollationReg;
type PendingPragma = reg::PragmaReg;
// 2.1.0 additive captures.
type PendingCopyHandler = reg::CopyHandlerReg;
type PendingSecret = reg::SecretReg;
// ADR-0029 Phase 6.2.d.2 — `pub` so crate::extension_wasmos
// can construct pending entries during interface migration. The
// underlying `reg::*` type is already public; the alias just
// preserves grep-locality with the private original.
pub type PendingSetting = reg::SettingReg;
type PendingTableMacro = reg::TableMacroReg;
type PendingModifiedType = reg::ModifiedTypeReg;
type PendingEnumType = reg::EnumTypeReg;
// 2.2.0 additive captures (Items 6-7).
type PendingScalarEx = reg::ScalarExReg;
type PendingConnCallback = reg::ConnCallbackReg;
type PendingCoordinateSystem = reg::CoordinateSystemReg;
type PendingArrowTable = reg::ArrowTableReg;
type PendingEncoding = reg::EncodingReg;
type PendingCompression = reg::CompressionReg;
// 2.3.0 / v3 additive captures.
// ADR-0029 Phase 6.2.d.2 — `pub` so crate::extension_wasmos can
// construct pending entries during interface migration.
pub type PendingParser = reg::ParserReg;
pub type PendingOptimizer = reg::OptimizerReg;
// 3.1.0 additive capture: streaming/filter-pushdown table function.
type PendingFilterableTable = reg::FilterableTableReg;

/// A log-storage sink registered by an extension. Keyed by `name` (the
/// storage name the guest declared); `callback_handle` routes every
/// log-storage callback back to the owning component.
///
/// Defined locally rather than in [`crate::reg`] because the WIT + host
/// register_* wiring is landing in a sibling agent's phase; a later
/// phase promotes this into `crate::reg` once the WIT surface stabilises.
#[derive(Clone, Debug)]
pub struct PendingLogStorage {
    pub name: String,
    pub callback_handle: u32,
}

#[derive(Default)]
struct PendingScalarRegistry {
    entries: Vec<PendingScalar>,
}

#[derive(Default)]
struct PendingTableRegistry {
    entries: Vec<PendingTable>,
}

#[derive(Default)]
struct PendingAggregateRegistry {
    entries: Vec<PendingAggregate>,
}

/// The full set of registrations captured from one or more components, ready
/// for a direction-specific sink to forward into the database.
///
/// `Clone` is derived so [`ExtensionManager::drain_pending_registrations`] can
/// stash a snapshot into `SiblingState::replay_archive` before returning the
/// drained value to the primary core. The archive lets a lazily-materialized
/// sibling core (Direction-1 §5.(b.1)) replay the primary's post-LOAD C-API
/// registrations against its own DuckDB catalog — see the Phase 4 follow-ups
/// note in `crates/ducklink-host/src/lib.rs`.
#[derive(Clone, Default)]
pub struct PendingRegistrationsData {
    pub scalars: Vec<PendingScalar>,
    pub tables: Vec<PendingTable>,
    pub aggregates: Vec<PendingAggregate>,
    pub macros: Vec<PendingMacro>,
    pub replacement_scans: Vec<PendingReplacementScan>,
    pub logical_types: Vec<PendingLogicalType>,
    pub casts: Vec<PendingCast>,
    pub storages: Vec<PendingStorage>,
    // Additive fields (Phase: drain-plumbing). These mirror pending_*
    // buffers on `ExtensionStoreState` that were already captured by the
    // register_* host impls but not previously carried through
    // `drain_pending` into the .duckdb_extension shim.
    pub pragmas: Vec<PendingPragma>,
    pub settings: Vec<PendingSetting>,
    /// Secret TYPE + PROVIDER registrations (Phase 3 host-import wiring). A
    /// secret-capable component declares its type(s)/provider(s) here via the
    /// `secret` interface; the host drains them into `ExtensionManager::secret_backends`
    /// so `CREATE SECRET (TYPE ..., PROVIDER ...)` can route to the backing
    /// extension's `secret-dispatch::create-secret` export.
    pub secrets: Vec<PendingSecret>,
    pub copy_handlers: Vec<PendingCopyHandler>,
    pub arrow_tables: Vec<PendingArrowTable>,
    pub scalar_ex: Vec<PendingScalarEx>,
    pub table_macros: Vec<PendingTableMacro>,
    pub enum_types: Vec<PendingEnumType>,
    pub modified_types: Vec<PendingModifiedType>,
    /// NEW additive sink (see [`PendingLogStorage`]). The register_* host
    /// impl and take/drain wiring for pushes into this buffer land in a
    /// sibling phase; the field is added here so the drain path is ready.
    pub log_storages: Vec<PendingLogStorage>,
    /// Coordinate reference systems captured by `HostCoordSystemRegistry`.
    ///
    /// Downstream consumer contract: the .duckdb_extension shim / reg_duckdb
    /// sink is responsible for deciding what to do with these SRID entries.
    /// DuckDB core exposes no first-class CRS registration API, so today the
    /// expected behavior is either (a) fail-loud so operators notice the
    /// unimplemented path, or (b) observe/log for diagnostics. What we
    /// guarantee here is that the entries no longer silently vanish between
    /// `register_coordinate_system` and `drain_pending`.
    pub coordinate_systems: Vec<PendingCoordinateSystem>,
}

impl PendingRegistrationsData {
    pub fn append(&mut self, mut other: PendingRegistrationsData) {
        self.scalars.append(&mut other.scalars);
        self.tables.append(&mut other.tables);
        self.aggregates.append(&mut other.aggregates);
        self.macros.append(&mut other.macros);
        self.replacement_scans.append(&mut other.replacement_scans);
        self.logical_types.append(&mut other.logical_types);
        self.casts.append(&mut other.casts);
        self.storages.append(&mut other.storages);
        // Additive fields (Phase: drain-plumbing).
        self.pragmas.append(&mut other.pragmas);
        self.settings.append(&mut other.settings);
        self.secrets.append(&mut other.secrets);
        self.copy_handlers.append(&mut other.copy_handlers);
        self.arrow_tables.append(&mut other.arrow_tables);
        self.scalar_ex.append(&mut other.scalar_ex);
        self.table_macros.append(&mut other.table_macros);
        self.enum_types.append(&mut other.enum_types);
        self.modified_types.append(&mut other.modified_types);
        self.log_storages.append(&mut other.log_storages);
        self.coordinate_systems
            .append(&mut other.coordinate_systems);
    }
}

pub fn summarize_registration_names<T, F>(entries: &[T], mut project: F) -> String
where
    F: FnMut(&T) -> &str,
{
    if entries.is_empty() {
        return "none".to_string();
    }
    const PREVIEW: usize = 3;
    let mut listed: Vec<String> = entries
        .iter()
        .take(PREVIEW)
        .map(|entry| project(entry).to_string())
        .collect();
    if entries.len() > PREVIEW {
        listed.push(format!("+{} more", entries.len() - PREVIEW));
    }
    listed.join(", ")
}

// ---------------------------------------------------------------------------
// ExtensionStoreState
// ---------------------------------------------------------------------------

/// Per-component wasmtime store data: wasi context + capability capture buffers
/// + the config/logging sink + the shared callback registry.
pub struct ExtensionStoreState {
    table: ResourceTable,
    wasi: WasiCtx,
    /// wasi:http host context. Present on every extension store so the shared
    /// `base_linker` can wire `wasi:http/{types,outgoing-handler}@0.2.9`. Only
    /// extensions whose composed component actually imports wasi:http (today:
    /// the s3-wasm-composed `cache.wasm`) exercise this; every other extension
    /// pays nothing beyond an unused `WasiHttpCtx` in its store.
    wasi_http: WasiHttpCtx,
    services: Box<dyn ExtensionServices>,
    next_resource_id: u32,
    scalar_registries: HashMap<u32, PendingScalarRegistry>,
    table_registries: HashMap<u32, PendingTableRegistry>,
    aggregate_registries: HashMap<u32, PendingAggregateRegistry>,
    // Registrations are retained here once their registry resource is dropped by
    // the guest (which happens as soon as `load()` returns), so they survive
    // until `drain_pending` forwards them to the sink.
    pending_scalars: Vec<PendingScalar>,
    pending_tables: Vec<PendingTable>,
    pending_aggregates: Vec<PendingAggregate>,
    pending_macros: Vec<PendingMacro>,
    pending_replacement_scans: Vec<PendingReplacementScan>,
    pending_logical_types: Vec<PendingLogicalType>,
    pending_casts: Vec<PendingCast>,
    pending_storages: Vec<PendingStorage>,
    pending_indexes: Vec<PendingIndex>,
    pending_files: Vec<PendingFiles>,
    pending_collations: Vec<PendingCollation>,
    pending_pragmas: Vec<PendingPragma>,
    // 2.1.0 additive capture buffers.
    pending_copy_handlers: Vec<PendingCopyHandler>,
    pending_secrets: Vec<PendingSecret>,
    pending_settings: Vec<PendingSetting>,
    pending_table_macros: Vec<PendingTableMacro>,
    pending_modified_types: Vec<PendingModifiedType>,
    pending_enum_types: Vec<PendingEnumType>,
    // 2.2.0 additive capture buffers (Items 6-7).
    pending_scalar_ex: Vec<PendingScalarEx>,
    pending_conn_callbacks: Vec<PendingConnCallback>,
    pending_coordinate_systems: Vec<PendingCoordinateSystem>,
    pending_arrow_tables: Vec<PendingArrowTable>,
    pending_encodings: Vec<PendingEncoding>,
    pending_compressions: Vec<PendingCompression>,
    // 2.3.0 / v3 additive capture buffers.
    pending_parsers: Vec<PendingParser>,
    pending_optimizers: Vec<PendingOptimizer>,
    pending_filterable_tables: Vec<PendingFilterableTable>,
    // Additive capture buffer (Phase: drain-plumbing). Populated by the
    // register_* host impl in a sibling phase; wired through
    // `drain_pending` now so no captures are dropped once that lands.
    pending_log_storages: Vec<PendingLogStorage>,
    /// Maps the handle returned from `table-registry.register` to the table
    /// function name, so `files.register-replacement-scan` can resolve it.
    table_handle_names: HashMap<u32, String>,
    callback_registry: Arc<RwLock<CallbackRegistry>>,
    extension_name: String,
    /// `Some(..)` only for a component that imports `compose:dynlink/linker`
    /// (the gate is in `load_component`); every other extension is unaffected
    /// and pays nothing. The bridge resolves/invokes the shared, resident
    /// provider (e.g. the one warmed ~38 MB pylon) on the guest's behalf.
    dynlink: Option<crate::compose_dynlink::DynLinkBridge>,
    /// Live `duckdb:extension/file-lock.lock-handle` resources held by the
    /// guest. Keyed by the `rep` id embedded in the wit-bindgen `Resource`
    /// handle (allocated via [`alloc_lock_handle`]); each value owns the OS
    /// file whose Drop releases the underlying `fs2::FileExt::lock_exclusive`
    /// flock. Empty for components that don't import file-lock.
    lock_handles: HashMap<u32, LockHandleState>,
    /// Monotonic counter for allocated file-lock handle ids. Never reused
    /// within a process (matches CallbackRegistry's handle policy so guests
    /// that stash a handle across an unusual code path don't accidentally
    /// alias a released one).
    next_lock_handle: u32,
}

impl ExtensionStoreState {
    pub fn new(
        wasi: WasiCtx,
        services: Box<dyn ExtensionServices>,
        callback_registry: Arc<RwLock<CallbackRegistry>>,
        extension_name: String,
    ) -> Self {
        Self::with_dynlink(wasi, services, callback_registry, extension_name, None)
    }

    /// Like [`new`](Self::new) but also carries an optional
    /// `compose:dynlink/linker` bridge (for a component that imports it).
    pub fn with_dynlink(
        wasi: WasiCtx,
        services: Box<dyn ExtensionServices>,
        callback_registry: Arc<RwLock<CallbackRegistry>>,
        extension_name: String,
        dynlink: Option<crate::compose_dynlink::DynLinkBridge>,
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            wasi,
            wasi_http: WasiHttpCtx::new(),
            services,
            next_resource_id: 1,
            scalar_registries: HashMap::new(),
            table_registries: HashMap::new(),
            aggregate_registries: HashMap::new(),
            pending_scalars: Vec::new(),
            pending_tables: Vec::new(),
            pending_aggregates: Vec::new(),
            pending_macros: Vec::new(),
            pending_replacement_scans: Vec::new(),
            pending_logical_types: Vec::new(),
            pending_casts: Vec::new(),
            pending_storages: Vec::new(),
            pending_indexes: Vec::new(),
            pending_files: Vec::new(),
            pending_collations: Vec::new(),
            pending_pragmas: Vec::new(),
            pending_copy_handlers: Vec::new(),
            pending_secrets: Vec::new(),
            pending_settings: Vec::new(),
            pending_table_macros: Vec::new(),
            pending_modified_types: Vec::new(),
            pending_enum_types: Vec::new(),
            pending_scalar_ex: Vec::new(),
            pending_conn_callbacks: Vec::new(),
            pending_coordinate_systems: Vec::new(),
            pending_arrow_tables: Vec::new(),
            pending_encodings: Vec::new(),
            pending_compressions: Vec::new(),
            pending_parsers: Vec::new(),
            pending_optimizers: Vec::new(),
            pending_filterable_tables: Vec::new(),
            pending_log_storages: Vec::new(),
            table_handle_names: HashMap::new(),
            callback_registry,
            extension_name,
            dynlink,
            lock_handles: HashMap::new(),
            next_lock_handle: 1,
        }
    }

    /// Allocate a new `file-lock.lock-handle` id and stash the owning
    /// [`LockHandleState`] under it. Handles are monotonic per store
    /// (never reused) so a component that stashes an id after `release`
    /// cannot accidentally alias a freshly-acquired lock.
    fn alloc_lock_handle(&mut self, state: LockHandleState) -> u32 {
        let id = self.next_lock_handle;
        self.next_lock_handle = self.next_lock_handle.wrapping_add(1).max(1);
        self.lock_handles.insert(id, state);
        id
    }

    /// Drop the state for `id` (releasing the flock via
    /// `LockHandleState::Drop`). No-op if the id was already released.
    fn free_lock_handle(&mut self, id: u32) {
        self.lock_handles.remove(&id);
    }

    /// Accessor for the dynlink bridge, used by `impl_compose_dynlink_host!`.
    /// Reached only after the `imports_linker` gate set `dynlink = Some(..)`,
    /// so the `expect` never fires for a component wired through that gate.
    fn dynlink_bridge(&mut self) -> &mut crate::compose_dynlink::DynLinkBridge {
        self.dynlink
            .as_mut()
            .expect("dynlink bridge present only when the component imports compose:dynlink/linker")
    }

    // ADR-0029 Phase 6.2.d.2 — visibility bumped from `fn` to
    // `pub fn` so `crate::extension_wasmos` can allocate resource
    // ids while migrating interfaces to `wasmos_runtime_api::
    // HostImports`. No behavior change; the invariant that the
    // returned id is nonzero + monotonic (wrapping) is preserved.
    pub fn alloc_resource_id(&mut self) -> u32 {
        let id = self.next_resource_id;
        self.next_resource_id = self.next_resource_id.wrapping_add(1).max(1);
        id
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — the extension's name, used
    /// by `crate::extension_wasmos` handlers for pending-buffer
    /// tagging. Read-only; the field is set at construction time
    /// via `Self::new` / `Self::with_dynlink`.
    pub fn extension_name(&self) -> &str {
        &self.extension_name
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_settings`.
    pub fn push_pending_setting(&mut self, setting: PendingSetting) {
        self.pending_settings.push(setting);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_parsers`.
    pub fn push_pending_parser(&mut self, parser: PendingParser) {
        self.pending_parsers.push(parser);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_optimizers`.
    pub fn push_pending_optimizer(&mut self, optimizer: PendingOptimizer) {
        self.pending_optimizers.push(optimizer);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_coordinate_systems`.
    pub fn push_pending_coordinate_system(&mut self, entry: reg::CoordinateSystemReg) {
        self.pending_coordinate_systems.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_storages`.
    pub fn push_pending_storage(&mut self, entry: reg::StorageReg) {
        self.pending_storages.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_log_storages`.
    pub fn push_pending_log_storage(&mut self, entry: PendingLogStorage) {
        self.pending_log_storages.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — allocate a globally-routable
    /// callback handle for the given kind, mapping the caller's
    /// dispatcher_handle. Delegates to the callback registry the
    /// same way `Self::allocate_callback_handle` does; kept as a
    /// visibility promotion so `crate::extension_wasmos` handlers
    /// can wire log-storage + filterable-table + similar globally-
    /// routed callbacks without duplicating the registry write.
    pub fn allocate_callback_handle_pub(
        &self,
        dispatcher_handle: u32,
        kind: crate::CallbackKind,
    ) -> u32 {
        self.allocate_callback_handle(dispatcher_handle, kind)
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — forward a read-only SELECT
    /// through the neutral `ExtensionServices::query` sink. Mirrors
    /// the wit-bindgen `extension_query::Host::query` body.
    pub fn services_query(&mut self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        self.services.query(sql)
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_secrets`.
    pub fn push_pending_secret(&mut self, entry: reg::SecretReg) {
        self.pending_secrets.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_table_macros`.
    pub fn push_pending_table_macro(&mut self, entry: reg::TableMacroReg) {
        self.pending_table_macros.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_modified_types`.
    pub fn push_pending_modified_type(&mut self, entry: reg::ModifiedTypeReg) {
        self.pending_modified_types.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_enum_types`.
    pub fn push_pending_enum_type(&mut self, entry: reg::EnumTypeReg) {
        self.pending_enum_types.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_replacement_scans`.
    pub fn push_pending_replacement_scan(&mut self, entry: reg::ReplacementScanReg) {
        self.pending_replacement_scans.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_copy_handlers`.
    pub fn push_pending_copy_handler(&mut self, entry: reg::CopyHandlerReg) {
        self.pending_copy_handlers.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_arrow_tables`.
    pub fn push_pending_arrow_table(&mut self, entry: reg::ArrowTableReg) {
        self.pending_arrow_tables.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_filterable_tables`.
    pub fn push_pending_filterable_table(&mut self, entry: reg::FilterableTableReg) {
        self.pending_filterable_tables.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_scalar_ex`.
    pub fn push_pending_scalar_ex(&mut self, entry: reg::ScalarExReg) {
        self.pending_scalar_ex.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_logical_types`.
    pub fn push_pending_logical_type(&mut self, entry: reg::LogicalTypeReg) {
        self.pending_logical_types.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_macros`.
    pub fn push_pending_macro(&mut self, entry: reg::MacroReg) {
        self.pending_macros.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — append to `pending_casts`.
    pub fn push_pending_cast(&mut self, entry: reg::CastReg) {
        self.pending_casts.push(entry);
    }

    /// ADR-0029 Phase 6.2.d.2-m accessor — acquire an exclusive
    /// advisory lock on `path`, stash the `LockHandleState`, and
    /// return the fresh handle id. Wraps the private
    /// `LockHandleState::acquire_exclusive` + `alloc_lock_handle`
    /// pair so `crate::extension_wasmos` can register the
    /// `file-lock.acquire-exclusive` handler without exposing the
    /// LockHandleState struct itself.
    pub fn acquire_exclusive_lock(&mut self, path: &str) -> Result<u32, String> {
        let state = LockHandleState::acquire_exclusive(path)?;
        Ok(self.alloc_lock_handle(state))
    }

    /// ADR-0029 Phase 6.2.d.2-m accessor — try-acquire variant of
    /// `acquire_exclusive_lock`. Returns `Ok(Some(id))` on
    /// acquisition, `Ok(None)` if the lock is currently held by
    /// another process, `Err` on IO failure.
    pub fn try_acquire_exclusive_lock(&mut self, path: &str) -> Result<Option<u32>, String> {
        match LockHandleState::try_acquire_exclusive(path)? {
            Some(state) => Ok(Some(self.alloc_lock_handle(state))),
            None => Ok(None),
        }
    }

    /// ADR-0029 Phase 6.2.d.2-m accessor — release the lock at
    /// `id` (drops the underlying `LockHandleState`, releasing the
    /// OS flock via its `Drop` impl). No-op if the id was already
    /// released. Wraps the private `free_lock_handle`.
    pub fn release_lock_handle(&mut self, id: u32) {
        self.free_lock_handle(id);
    }

    /// ADR-0029 Phase 6.2.g accessor — count of currently-held file
    /// lock handles. Non-mutating inspection helper for stateful
    /// integration tests + operational counters (e.g. surfacing
    /// "extension X holds N file locks" in a diagnostic dump).
    /// Zero for extensions that don't import `file-lock`.
    pub fn active_lock_handle_count(&self) -> usize {
        self.lock_handles.len()
    }

    /// ADR-0029 Phase 6.2.g accessor — true when `id` currently
    /// names a live lock-handle registration. Sibling of
    /// [`active_lock_handle_count`](Self::active_lock_handle_count)
    /// for tests that need to verify a specific handle survived /
    /// was released.
    pub fn contains_lock_handle(&self, id: u32) -> bool {
        self.lock_handles.contains_key(&id)
    }

    /// ADR-0029 Phase 6.2.g accessor — clone-out of the shared
    /// callback registry `Arc`. Read-only for tests that need to
    /// verify a callback handle was allocated / released after a
    /// SyncHostCall dispatch (the private field is per-instance;
    /// cloning the Arc is cheap and lets a test hold a live read
    /// handle without threading the registry through the fixture
    /// twice).
    pub fn callback_registry_handle(&self) -> Arc<RwLock<crate::CallbackRegistry>> {
        Arc::clone(&self.callback_registry)
    }

    /// ADR-0029 Phase 6.2.g accessor — current length of the
    /// `pending_parsers` capture buffer. Stateful-integration
    /// tests use it to assert that a wasmos-side
    /// `register-parser-extension` dispatch actually pushed to
    /// the buffer without exposing the field itself. Sibling of
    /// [`active_lock_handle_count`](Self::active_lock_handle_count)
    /// for the parser-registration capture path.
    pub fn pending_parser_count(&self) -> usize {
        self.pending_parsers.len()
    }

    /// ADR-0029 Phase 6.2.d.2-o accessor — allocate a fresh
    /// scalar-registry id + insert a default `PendingScalarRegistry`.
    /// Returned id is the rep for a `runtime.scalar-registry`
    /// Resource<T> handed back to the guest via
    /// `runtime.get-capability(scalar)`.
    pub fn init_scalar_registry(&mut self) -> u32 {
        let id = self.alloc_resource_id();
        self.scalar_registries
            .insert(id, PendingScalarRegistry::default());
        id
    }

    /// ADR-0029 Phase 6.2.d.2-o accessor — sibling of
    /// `init_scalar_registry` for `runtime.table-registry`.
    pub fn init_table_registry(&mut self) -> u32 {
        let id = self.alloc_resource_id();
        self.table_registries
            .insert(id, PendingTableRegistry::default());
        id
    }

    /// ADR-0029 Phase 6.2.d.2-o accessor — sibling of
    /// `init_scalar_registry` for `runtime.aggregate-registry`.
    pub fn init_aggregate_registry(&mut self) -> u32 {
        let id = self.alloc_resource_id();
        self.aggregate_registries
            .insert(id, PendingAggregateRegistry::default());
        id
    }

    /// ADR-0029 Phase 6.2.d.2-p accessor — release a callback
    /// handle. Wraps the private `release_callback_handle` so
    /// `crate::extension_wasmos`'s XxxCallback drop handlers can
    /// clean up the global registry when a callback resource is
    /// dropped by the guest. (Actual invocation is subject to the
    /// wasmos-side destructor-dispatch gap — see Phase 6.2.d.2-n.)
    pub fn release_callback_handle_pub(&self, handle: u32) {
        self.release_callback_handle(handle);
    }

    /// ADR-0029 Phase 6.2.d.2-q — kind-mismatch / unknown-handle /
    /// unknown-registry errors that XxxRegistry.register handlers
    /// map to `Duckerror::Invalidargument` / `Duckerror::Internal`.
    /// Kept as a concise enum here so `crate::extension_wasmos`
    /// translates once at the boundary.
    ///
    /// Not `Debug` / `Display` — the wasmos handlers pattern-match
    /// on this enum and produce the exact wire message the
    /// wit-bindgen counterpart uses.

    /// Validate that `callback_handle` maps to a callback registry
    /// entry of the given `expected` kind. Returns Ok on match,
    /// distinct errors on kind-mismatch vs unknown-handle so
    /// callers surface the right Duckerror variant.
    pub fn validate_callback_kind(
        &self,
        callback_handle: u32,
        expected: crate::CallbackKind,
    ) -> Result<(), CallbackValidationError> {
        let registry = self
            .callback_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        match registry.get(callback_handle) {
            Some(entry) if entry.kind == expected => Ok(()),
            Some(_) => Err(CallbackValidationError::KindMismatch),
            None => Err(CallbackValidationError::UnknownHandle),
        }
    }

    /// ADR-0029 Phase 6.2.d.2-q — push a fully-converted
    /// `reg::ScalarReg` into the per-registry buffer at
    /// `registry_id` + return a fresh alloc'd resource id.
    /// Assumes the callback kind has already been validated via
    /// `validate_callback_kind`. Fails only on unknown registry.
    pub fn scalar_registry_push(
        &mut self,
        registry_id: u32,
        entry: reg::ScalarReg,
    ) -> Result<u32, RegistryPushError> {
        let registry = self
            .scalar_registries
            .get_mut(&registry_id)
            .ok_or(RegistryPushError::UnknownRegistry)?;
        registry.entries.push(entry);
        Ok(self.alloc_resource_id())
    }

    /// ADR-0029 Phase 6.2.d.2-q — sibling of
    /// `scalar_registry_push` for tables. Also updates
    /// `table_handle_names` with the returned handle so
    /// `files.register-replacement-scan` can resolve the name.
    pub fn table_registry_push(
        &mut self,
        registry_id: u32,
        entry: reg::TableReg,
    ) -> Result<u32, RegistryPushError> {
        let table_name = entry.name.clone();
        let registry = self
            .table_registries
            .get_mut(&registry_id)
            .ok_or(RegistryPushError::UnknownRegistry)?;
        registry.entries.push(entry);
        let handle = self.alloc_resource_id();
        self.table_handle_names.insert(handle, table_name);
        Ok(handle)
    }

    /// ADR-0029 Phase 6.2.d.2-q — sibling of
    /// `scalar_registry_push` for aggregates.
    pub fn aggregate_registry_push(
        &mut self,
        registry_id: u32,
        entry: reg::AggregateReg,
    ) -> Result<u32, RegistryPushError> {
        let registry = self
            .aggregate_registries
            .get_mut(&registry_id)
            .ok_or(RegistryPushError::UnknownRegistry)?;
        registry.entries.push(entry);
        Ok(self.alloc_resource_id())
    }

    /// ADR-0029 Phase 6.2.d.2-q — pragma has NO per-registry
    /// buffer; the wit-bindgen counterpart pushes directly to
    /// `pending_pragmas` from `register_call`. Kept as a distinct
    /// accessor so the wasmos handler mirrors that shape.
    pub fn pragma_registry_push_call(&mut self, entry: reg::PragmaReg) -> u32 {
        self.pending_pragmas.push(entry);
        self.alloc_resource_id()
    }

    /// ADR-0029 Phase 6.2.d.2-q — drain the per-registry buffer at
    /// `rep` into `pending_scalars`, then remove the registry.
    /// Matches the wit-bindgen `HostScalarRegistry::drop`
    /// behaviour at `crate::extension` line 1633.
    ///
    /// Wasmos-side dispatch of `[resource-drop]...` is subject to
    /// the same destructor-gap noted for other resources — this
    /// handler is dead code until the adapter gap closes.
    pub fn drain_scalar_registry(&mut self, rep: u32) {
        if let Some(registry) = self.scalar_registries.remove(&rep) {
            self.pending_scalars.extend(registry.entries);
        }
    }

    /// ADR-0029 Phase 6.2.d.2-q — sibling of
    /// `drain_scalar_registry` for tables.
    pub fn drain_table_registry(&mut self, rep: u32) {
        if let Some(registry) = self.table_registries.remove(&rep) {
            self.pending_tables.extend(registry.entries);
        }
    }

    /// ADR-0029 Phase 6.2.d.2-q — sibling of
    /// `drain_scalar_registry` for aggregates.
    pub fn drain_aggregate_registry(&mut self, rep: u32) {
        if let Some(registry) = self.aggregate_registries.remove(&rep) {
            self.pending_aggregates.extend(registry.entries);
        }
    }
}

// ADR-0029 Phase 6.2.d.2-q — concise error enums for the registry
// accessors. `crate::extension_wasmos` pattern-matches these to
// produce the exact wit-bindgen counterpart's Duckerror wire
// messages (Invalidargument vs Internal).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackValidationError {
    /// Callback exists but its kind doesn't match the expected one.
    /// Maps to `Duckerror::Invalidargument("callback handle is not
    /// <kind>")`.
    KindMismatch,
    /// No callback registered under this handle. Maps to
    /// `Duckerror::Internal("unknown <kind> callback handle")`.
    UnknownHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryPushError {
    /// No per-registry buffer exists for this handle. Maps to
    /// `Duckerror::Internal("unknown <kind> registry handle")`.
    UnknownRegistry,
}

impl ExtensionStoreState {

    /// ADR-0029 Phase 6.2.d.2 accessor — look up the table function
    /// name that was registered for a given handle. Used by the
    /// `files.register_replacement_scan` handler to resolve the
    /// scan's `table-function` handle to the underlying name. Returns
    /// `None` if the handle was never registered (the wit-bindgen
    /// counterpart returns Err in that case).
    pub fn lookup_table_handle_name(&self, handle: u32) -> Option<String> {
        self.table_handle_names.get(&handle).cloned()
    }

    /// ADR-0029 Phase 6.2.d.2 accessor — mutable borrow of the
    /// neutral `ExtensionServices` trait object. Lets
    /// `crate::extension_wasmos` handlers reach every services
    /// method (provider_version, list_keys, get_string/i64/etc,
    /// log, log_fields, nested_exec, ...) without one delegator
    /// per method.
    pub fn services_mut(&mut self) -> &mut dyn ExtensionServices {
        &mut *self.services
    }

    fn allocate_callback_handle(&self, dispatcher_handle: u32, kind: CallbackKind) -> u32 {
        let mut registry = self
            .callback_registry
            .write()
            .unwrap_or_else(|e| e.into_inner());
        registry.allocate(&self.extension_name, kind, dispatcher_handle)
    }

    fn release_callback_handle(&self, handle: u32) {
        let mut registry = self
            .callback_registry
            .write()
            .unwrap_or_else(|e| e.into_inner());
        registry.remove(handle);
    }

    /// Drains ONLY the captured storage-backend registrations, leaving every
    /// other pending registration (scalars/tables/...) intact for the normal
    /// `drain_pending` hook flow. Used right after `load()` so an ATTACH backend
    /// is routable before the core ever drains function registrations.
    fn take_pending_storages(&mut self) -> Vec<PendingStorage> {
        std::mem::take(&mut self.pending_storages)
    }

    /// Item 3 / M2a: drains ONLY the captured custom-index TYPE registrations,
    /// used right after `load()` so the host can surface them to the core (which
    /// pulls the list via `index-host.index-type-list` and registers a wasm
    /// IndexType for each, routing `CREATE INDEX ... USING <type>` to the
    /// component's index-dispatch export).
    fn take_pending_indexes(&mut self) -> Vec<PendingIndex> {
        std::mem::take(&mut self.pending_indexes)
    }

    /// Drains ONLY the captured files-backend registrations (httpfs M2), used
    /// right after `load()` so the host knows which component backs http(s)
    /// reads before any query runs.
    fn take_pending_files(&mut self) -> Vec<PendingFiles> {
        std::mem::take(&mut self.pending_files)
    }

    /// Drains ONLY the captured collation registrations (Item 2), used right
    /// after `load()` so the host can surface them to the core (the
    /// `collation-host` pull-back interface for this capability was never
    /// produced; enumeration goes through `PendingRegistrationsData` instead),
    /// wrapping each as a DuckDB collation reusing the already-registered
    /// sort-key scalar.
    fn take_pending_collations(&mut self) -> Vec<PendingCollation> {
        std::mem::take(&mut self.pending_collations)
    }

    /// Item 4: drains ONLY the captured pragma registrations, used right after
    /// `load()` so the host can surface them to the core (the `pragma-host`
    /// pull-back interface for this capability was never produced; enumeration
    /// goes through `PendingRegistrationsData` instead), where the core
    /// intercepts `PRAGMA <name>(...)`.
    fn take_pending_pragmas(&mut self) -> Vec<PendingPragma> {
        std::mem::take(&mut self.pending_pragmas)
    }

    // --- 2.1.0 additive drains (mirror take_pending_pragmas) ---
    fn take_pending_copy_handlers(&mut self) -> Vec<PendingCopyHandler> {
        std::mem::take(&mut self.pending_copy_handlers)
    }
    fn take_pending_secrets(&mut self) -> Vec<PendingSecret> {
        std::mem::take(&mut self.pending_secrets)
    }
    fn take_pending_settings(&mut self) -> Vec<PendingSetting> {
        std::mem::take(&mut self.pending_settings)
    }
    fn take_pending_table_macros(&mut self) -> Vec<PendingTableMacro> {
        std::mem::take(&mut self.pending_table_macros)
    }
    fn take_pending_modified_types(&mut self) -> Vec<PendingModifiedType> {
        std::mem::take(&mut self.pending_modified_types)
    }
    fn take_pending_enum_types(&mut self) -> Vec<PendingEnumType> {
        std::mem::take(&mut self.pending_enum_types)
    }

    // --- 2.2.0 additive drains (Items 6-7; mirror the 2.1.0 drains) ---
    fn take_pending_scalar_ex(&mut self) -> Vec<PendingScalarEx> {
        std::mem::take(&mut self.pending_scalar_ex)
    }
    fn take_pending_conn_callbacks(&mut self) -> Vec<PendingConnCallback> {
        std::mem::take(&mut self.pending_conn_callbacks)
    }
    fn take_pending_coordinate_systems(&mut self) -> Vec<PendingCoordinateSystem> {
        std::mem::take(&mut self.pending_coordinate_systems)
    }
    fn take_pending_arrow_tables(&mut self) -> Vec<PendingArrowTable> {
        std::mem::take(&mut self.pending_arrow_tables)
    }
    fn take_pending_encodings(&mut self) -> Vec<PendingEncoding> {
        std::mem::take(&mut self.pending_encodings)
    }
    fn take_pending_compressions(&mut self) -> Vec<PendingCompression> {
        std::mem::take(&mut self.pending_compressions)
    }

    // --- 2.3.0 / v3 additive drains ---
    fn take_pending_parsers(&mut self) -> Vec<PendingParser> {
        std::mem::take(&mut self.pending_parsers)
    }
    fn take_pending_optimizers(&mut self) -> Vec<PendingOptimizer> {
        std::mem::take(&mut self.pending_optimizers)
    }
    // --- 3.1.0 additive drain ---
    fn take_pending_filterable_tables(&mut self) -> Vec<PendingFilterableTable> {
        std::mem::take(&mut self.pending_filterable_tables)
    }

    // --- Phase: drain-plumbing additive drain (mirror take_pending_pragmas) ---
    fn take_pending_log_storages(&mut self) -> Vec<PendingLogStorage> {
        std::mem::take(&mut self.pending_log_storages)
    }

    fn drain_pending(&mut self) -> PendingRegistrationsData {
        // Combine registrations retained from dropped registries with any that
        // belong to registries still held alive by the guest.
        let mut scalars = std::mem::take(&mut self.pending_scalars);
        scalars.extend(
            self.scalar_registries
                .drain()
                .flat_map(|(_, registry)| registry.entries),
        );
        let mut tables = std::mem::take(&mut self.pending_tables);
        tables.extend(
            self.table_registries
                .drain()
                .flat_map(|(_, registry)| registry.entries),
        );
        let mut aggregates = std::mem::take(&mut self.pending_aggregates);
        aggregates.extend(
            self.aggregate_registries
                .drain()
                .flat_map(|(_, registry)| registry.entries),
        );
        let macros = std::mem::take(&mut self.pending_macros);
        let replacement_scans = std::mem::take(&mut self.pending_replacement_scans);
        let logical_types = std::mem::take(&mut self.pending_logical_types);
        let casts = std::mem::take(&mut self.pending_casts);
        let storages = std::mem::take(&mut self.pending_storages);
        // Additive drains (Phase: drain-plumbing). These were previously
        // captured but never forwarded into `PendingRegistrationsData`;
        // draining them here plugs the leak between the runtime and the
        // .duckdb_extension shim without touching any register_* host impl.
        let pragmas = self.take_pending_pragmas();
        let settings = self.take_pending_settings();
        let secrets = self.take_pending_secrets();
        let copy_handlers = self.take_pending_copy_handlers();
        let arrow_tables = self.take_pending_arrow_tables();
        let scalar_ex = self.take_pending_scalar_ex();
        let table_macros = self.take_pending_table_macros();
        let enum_types = self.take_pending_enum_types();
        let modified_types = self.take_pending_modified_types();
        let log_storages = self.take_pending_log_storages();
        let coordinate_systems = self.take_pending_coordinate_systems();
        let pending = PendingRegistrationsData {
            scalars,
            tables,
            aggregates,
            macros,
            replacement_scans,
            logical_types,
            casts,
            storages,
            pragmas,
            settings,
            secrets,
            copy_handlers,
            arrow_tables,
            scalar_ex,
            table_macros,
            enum_types,
            modified_types,
            log_storages,
            coordinate_systems,
        };
        let scalar_names =
            summarize_registration_names(&pending.scalars, |entry| entry.name.as_str());
        let table_names =
            summarize_registration_names(&pending.tables, |entry| entry.name.as_str());
        let aggregate_names =
            summarize_registration_names(&pending.aggregates, |entry| entry.name.as_str());
        let macro_names =
            summarize_registration_names(&pending.macros, |entry| entry.name.as_str());
        verbose_log!(
            "[extension-runtime:{}] draining pending registrations: scalars={} ({scalar_names}), tables={} ({table_names}), aggregates={} ({aggregate_names}), macros={} ({macro_names})",
            self.extension_name,
            pending.scalars.len(),
            pending.tables.len(),
            pending.aggregates.len(),
            pending.macros.len()
        );
        pending
    }
}

impl WasiView for ExtensionStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for ExtensionStoreState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.wasi_http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

impl wasmtime::component::HasData for ExtensionStoreState {
    type Data<'a> = &'a mut ExtensionStoreState;
}

// Satisfy a guest's `compose:dynlink/linker` import by delegating to the ONE
// bridge implementation (resolve/invoke against the shared, resident provider
// registry). Only components that actually import the linker get the host
// import added (the `imports_linker` gate in `load_component`).
crate::impl_compose_dynlink_host!(ExtensionStoreState, dynlink_bridge);


// Phase 6.2.l.2 — the empty `impl extension_types::Host for
// ExtensionStoreState {}` retired here. The base `types` interface
// has no host imports (it only defines records + variants), so the
// Host trait was empty. The wasmos-native `TypesHost` in
// extension_wasmos.rs is the equivalent zero-method marker.

#[cfg(test)]
impl ExtensionStoreState {
    // Item 4: a component declares a PRAGMA in `load()`. The host captures its
    // name + the callback handle into the neutral pending buffer; the core
    // later pulls the list via `drain_pending`'s `pragmas` field on
    // `PendingRegistrationsData` (the `pragma-host` pull-back interface for
    // this capability was never produced), intercepts `PRAGMA <name>(...)`,
    // dispatches via callback-dispatch.call-pragma (the component RETURNS a
    // SQL script as text), and runs that script.
    /// Phase 6.2.n Session 4 — signature inlined. The wit-bindgen
    /// `Resource<PragmaRegistry>` / `Resource<PragmaCallback>` args
    /// carried opaque u32 handle reps; test callers pass raw u32s.
    /// The `arguments` / `returns` / `options` args were unused
    /// (validation only exercised the callback-handle lookup path)
    /// and dropped.
    pub(crate) fn register_call(
        &mut self,
        _registry: u32,
        name: String,
        callback_handle: u32,
    ) -> Result<u32, Duckerror> {
        {
            let registry = self
                .callback_registry
                .read()
                .unwrap_or_else(|e| e.into_inner());
            match registry.get(callback_handle) {
                Some(entry) if entry.kind == CallbackKind::Pragma => {}
                Some(_) => {
                    return Err(Duckerror::Invalidargument(
                        "callback handle is not a pragma".to_string(),
                    ))
                }
                None => {
                    return Err(Duckerror::Internal(
                        "unknown pragma callback handle".to_string(),
                    ))
                }
            }
        }

        verbose_log!(
            "[extension-runtime:{}] registered pragma '{name}' (callback={callback_handle})",
            self.extension_name
        );
        self.pending_pragmas.push(PendingPragma {
            extension: self.extension_name.clone(),
            name,
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }

    // Phase 6.2.l.2 — the no-op `HostPragmaRegistry::drop` was
    // deleted here (never called; the file_lock migration below
    // also promoted a `drop` method, and two inherent `drop`s on
    // the same type would collide).
}

// Phase 6.2.l.2 — `impl extension_config::Host` and
// `impl extension_logging::Host` retired here. No test in this
// crate exercised them; the wasmos-native `ConfigHost` and
// `LoggingHost` in extension_wasmos.rs delegate to the same
// `self.services.*` sink methods on the production dispatch
// path. The `ExtensionServices` trait itself is unchanged.

// The `catalog` and `files` interfaces are part of the extension world so that
// extensions can register logical types, casts, macros, replacement scans, and
// copy handlers. The host satisfies the imports here so such extensions
// instantiate and load; the requests are captured into the neutral pending
// buffers. Forwarding them into DuckDB is the direction-specific sink's job.
// Phase 6.2.n Session 3 — catalog register-* inherent method
// signatures inlined (were `catalog::LogicalType` / `catalog::
// MacroDef` records). Same test coverage, no wit-bindgen types.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_logical_type(&mut self, name: String, physical: String) -> Result<u32, String> {
        let handle = self.alloc_resource_id();
        verbose_log!(
            "[extension-manager] catalog register-logical-type '{}' (physical={}) for '{}' -> handle {handle}",
            name, physical, self.extension_name
        );
        self.pending_logical_types.push(PendingLogicalType {
            extension: self.extension_name.clone(),
            name,
            physical,
        });
        Ok(handle)
    }

    pub(crate) fn register_macro(
        &mut self,
        schema: String,
        name: String,
        parameters: Vec<String>,
        definition_sql: String,
    ) -> Result<(), String> {
        verbose_log!(
            "[extension-manager] catalog register-macro '{}.{}' ({} params) for '{}'",
            schema, name, parameters.len(), self.extension_name
        );
        self.pending_macros.push(PendingMacro {
            extension: self.extension_name.clone(),
            schema,
            name,
            parameters,
            definition_sql,
        });
        Ok(())
    }
}

// Phase 6.2.n Session 3 — files register-* signatures inlined
// (were `files::ReplacementScan` / `files::CopyHandler` records).
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_replacement_scan(
        &mut self,
        table_function: u32,
        extensions: Vec<String>,
        // `mode` was `files::DetectionMode` enum — Debug-print only,
        // so a string works for the log line without losing content.
        mode: &str,
    ) -> Result<u32, String> {
        let function_name = self
            .table_handle_names
            .get(&table_function)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "replacement scan references unknown table-function handle {}",
                    table_function
                )
            })?;
        let id = self.alloc_resource_id();
        verbose_log!(
            "[extension-manager] files register-replacement-scan exts={:?} ({}) -> '{}' for '{}' (id {id})",
            extensions, mode, function_name, self.extension_name
        );
        self.pending_replacement_scans.push(PendingReplacementScan {
            extension: self.extension_name.clone(),
            extensions,
            function_name,
        });
        Ok(id)
    }

    pub(crate) fn register_copy_handler(
        &mut self,
        extension: String,
        function: u32,
    ) -> Result<u32, String> {
        let id = self.alloc_resource_id();
        verbose_log!(
            "[extension-manager] files register-copy-handler ext='{}' (function={}) for '{}' -> id {id}",
            extension, function, self.extension_name
        );
        self.pending_copy_handlers.push(PendingCopyHandler {
            extension: self.extension_name.clone(),
            file_extension: extension,
            function_handle: function,
        });
        Ok(id)
    }
}

// 2.1.0 (Item 2): the `secret` interface lets a component declare a secret TYPE
// and named PROVIDERs in `load()`. The host satisfies the import so
// secret-capable components instantiate; the declaration is captured into the
// neutral pending buffer. Materializing a concrete secret is driven through the
// component's exported `secret-dispatch`.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_secret_type(
        &mut self,
        type_name: String,
        // Phase 6.2.n Session 3 — the wit-bindgen `SecretParam` record
        // (name + redacted) is inlined as a plain `(String, bool)`
        // tuple so no wit-bindgen type crosses the signature.
        params: Vec<(String, bool)>,
        callback_handle: u32,
    ) -> Result<u32, Duckerror> {
        // Phase 3 (@5 host-import wiring): capture the secret TYPE declaration
        // into the neutral pending buffer. The host drains it into
        // `ExtensionManager::secret_backends` so `CREATE SECRET (TYPE ...)` can
        // route to this component's `secret-dispatch::create-secret` export
        // via `ExtensionInstance::create_secret`. Returns a locally-allocated
        // resource id (opaque to the guest) mirroring the pattern used for
        // parser / optimizer / filterable-table registration.
        let registry_id = self.alloc_resource_id();
        verbose_log!(
            "[extension-runtime:{}] registered secret type '{type_name}' \
             (registry={registry_id}, callback={callback_handle}, params={})",
            self.extension_name,
            params.len()
        );
        self.pending_secrets.push(PendingSecret {
            extension: self.extension_name.clone(),
            type_name,
            provider: None,
            params,
            callback_handle,
        });
        Ok(registry_id)
    }

    pub(crate) fn register_secret_provider(
        &mut self,
        type_name: String,
        provider: String,
        callback_handle: u32,
    ) -> Result<u32, Duckerror> {
        // Phase 3 (@5 host-import wiring): capture a named PROVIDER for an
        // already-declared secret TYPE (e.g. the "credential_chain" provider
        // for "s3"). Drained into `ExtensionManager::secret_backends` keyed by
        // `(type_name, Some(provider))`.
        let registry_id = self.alloc_resource_id();
        verbose_log!(
            "[extension-runtime:{}] registered secret provider '{type_name}'/'{provider}' \
             (registry={registry_id}, callback={callback_handle})",
            self.extension_name
        );
        self.pending_secrets.push(PendingSecret {
            extension: self.extension_name.clone(),
            type_name,
            provider: Some(provider),
            params: Vec::new(),
            callback_handle,
        });
        Ok(registry_id)
    }
}

// 2.1.0 (Item 3): the `settings` interface lets a component DECLARE a config
// option (distinct from reading config via `config`). Captured into the neutral
// pending buffer; the direction-specific sink surfaces it to the database.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_option(
        &mut self,
        name: String,
        description: String,
        // Phase 6.2.n Session 3 — wit-bindgen `SettingType` /
        // `SettingScope` enums replaced by pre-rendered strings
        // (the wire values). Both were 4-arm / 2-arm C-enums with no
        // payload; passing the string skips the enum → String render.
        ty: String,
        default_value: Option<String>,
        scope: String,
    ) -> Result<(), Duckerror> {
        verbose_log!(
            "[extension-runtime:{}] registered option '{name}' (type={ty}, scope={scope})",
            self.extension_name
        );
        self.pending_settings.push(PendingSetting {
            extension: self.extension_name.clone(),
            name,
            description,
            ty,
            default_value,
            scope,
        });
        Ok(())
    }
}

// Phase 6.2.l.2 — the deprecated `impl extension_parser::Host::
// register_parser_extension` and `impl extension_optimizer::Host::
// register_optimizer_rule` impls retired here. Both were flagged
// DEPRECATED (ducklink 5.0.0) in their docstrings: no host drains
// `pending_parsers` / `pending_optimizers` anymore, and no test in
// this crate exercised them. The wasmos-native `ParserHost` and
// `OptimizerHost` in extension_wasmos.rs still capture registrations
// on the production path (behavior preserved for any legacy
// components that still call `register-parser-extension` /
// `register-optimizer-rule`), so retiring these test-scaffolding
// impls only removes an unused Rust surface.

// DEPRECATED (ducklink 5.0.0) — scheduled for removal at the next
// `duckdb:extension` major bump. No host drains
// `pending_filterable_tables` anymore; components calling
// `register_filterable_table` still succeed but their declarations never
// reach DuckDB. Register through `runtime.table-registry` instead; DuckDB
// filters above the scan (correct, not pushdown-fast). See ducklink v4.6.0.
//
// Phase 6.2.l.2 — `impl extension_table_stream::Host::
// register_filterable_table` retired here (no test in this crate
// exercised it). The wasmos-native `TableStreamHost` in
// extension_wasmos.rs still captures registrations on the
// production path; retiring only removes an unused Rust surface.

// 2.1.0 (Item 5): the `macro-ext` interface adds TABLE macros (a relation body)
// on top of the existing scalar-macro registration.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_table_macro(
        &mut self,
        schema: String,
        name: String,
        parameters: Vec<String>,
        body_sql: String,
    ) -> Result<(), Duckerror> {
        verbose_log!(
            "[extension-runtime:{}] registered table macro '{schema}.{name}' ({} params)",
            self.extension_name,
            parameters.len()
        );
        self.pending_table_macros.push(PendingTableMacro {
            extension: self.extension_name.clone(),
            schema,
            name,
            parameters: parameters.into_iter().collect(),
            body_sql,
        });
        Ok(())
    }
}

// 2.1.0 (Item 5): the `types-ext` interface adds modified logical types (over a
// type-expression, riding the escape hatch) and ENUM types. `types` stays FROZEN.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_logical_type_modified(
        &mut self,
        name: String,
        type_expr: String,
    ) -> Result<u32, Duckerror> {
        verbose_log!(
            "[extension-runtime:{}] registered modified logical type '{name}' = {type_expr}",
            self.extension_name
        );
        self.pending_modified_types.push(PendingModifiedType {
            extension: self.extension_name.clone(),
            name,
            type_expr,
        });
        Ok(self.alloc_resource_id())
    }

    pub(crate) fn register_enum(
        &mut self,
        name: String,
        members: Vec<String>,
    ) -> Result<u32, Duckerror> {
        verbose_log!(
            "[extension-runtime:{}] registered enum type '{name}' ({} members)",
            self.extension_name,
            members.len()
        );
        self.pending_enum_types.push(PendingEnumType {
            extension: self.extension_name.clone(),
            name,
            members,
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 6): the `runtime-ext` interface adds a RICHER scalar registration
// (varargs + named args + NULL handling) without touching the frozen `runtime`
// scalar-registry signature. Captured into the neutral pending buffer; the
// direction-specific sink forwards it. A callback handle is allocated exactly
// like the base scalar path so invocations route to the owning component.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_scalar_ex(
        &mut self,
        name: String,
        // Phase 6.2.n Session 3 — signatures use the Phase 6.2.m
        // mirrors on `crate::extension`; `null_handling` is a plain
        // bool (`true` = Special, `false` = default short-circuit)
        // since the wit-bindgen enum had only those two arms.
        arguments: Vec<Funcarg>,
        varargs: Option<Logicaltype>,
        returns: Logicaltype,
        special_null: bool,
        callback_handle: u32,
        options: Option<Funcopts>,
    ) -> Result<u32, Duckerror> {
        // Phase 6.2.o — mirrors go straight into the neutral
        // reg::* helpers; no more wit-bindgen roundtrip.
        let arguments = convert_extension_funcargs(arguments);
        let varargs = varargs.map(convert_extension_logicaltype);
        let returns = convert_extension_logicaltype(returns);
        let options = options.map(convert_extension_funcopts);
        // WIT `funcflags` has no VOLATILE bit; derive it from the incoming
        // attributes: a scalar-ex is VOLATILE iff it did NOT declare
        // `deterministic`. Absent options -> non-volatile default, closing the
        // audit finding that treated every ex-path fn as VOLATILE.
        let volatile = options
            .as_ref()
            .map(|o| !o.attributes.deterministic)
            .unwrap_or(false);
        let registry_id = self.alloc_resource_id();
        verbose_log!(
            "[extension-runtime:{}] registered scalar-ex '{name}' (registry={registry_id}, callback={callback_handle}, varargs={}, special_null={special_null}, volatile={volatile})",
            self.extension_name,
            varargs.is_some()
        );
        self.pending_scalar_ex.push(PendingScalarEx {
            extension: self.extension_name.clone(),
            name,
            arguments,
            varargs,
            returns,
            special_null,
            volatile,
            callback_handle,
            options,
        });
        Ok(registry_id)
    }
}

// Phase 6.2.l.2 — extension_lifecycle::Host pure Unsupported-stub
// impl retired here. The wasmos-native LifecycleHost in
// extension_wasmos.rs returns the same error on the production
// path and is covered by `register_connection_callback_returns_
// unsupported`.

// 2.2.0 (Item 7): the `coordinate-system` interface lets a spatial component
// declare CRS definitions (authority + code + WKT2) in load(); the host captures
// them so the core can resolve geometry SRIDs. Registration only -- reprojection
// (GDAL/PROJ ST_Transform) is OUT OF SCOPE for 2.2.0.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_coordinate_system(
        &mut self,
        auth_name: String,
        code: u32,
        wkt: String,
    ) -> Result<u32, Duckerror> {
        verbose_log!(
            "[extension-runtime:{}] registered coordinate system {}:{}",
            self.extension_name, auth_name, code
        );
        self.pending_coordinate_systems
            .push(PendingCoordinateSystem {
                extension: self.extension_name.clone(),
                auth_name,
                code,
                wkt,
            });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 7): the `arrow-ext` interface lets a component declare an Arrow
// table producer; the host captures the declaration and streams the batches via
// the producer's callback handle (reusing the table cursor shape).
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_arrow_table(
        &mut self,
        name: String,
        // Phase 6.2.n Session 3 — `arrow_ext::Columndef` re-uses
        // types.wit's Columndef via wit-bindgen `use`, so the mirror
        // is a same-shape drop-in.
        schema: Vec<Columndef>,
        callback_handle: u32,
    ) -> Result<u32, Duckerror> {
        // Phase 6.2.o — direct mirror → reg::* conversion.
        let columns = convert_extension_columndefs(schema);
        verbose_log!(
            "[extension-runtime:{}] registered arrow table '{name}' ({} columns, callback={callback_handle})",
            self.extension_name,
            columns.len()
        );
        self.pending_arrow_tables.push(PendingArrowTable {
            extension: self.extension_name.clone(),
            name,
            columns,
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }
}

// Phase 6.2.l.2 — the extension_encoding::Host and
// extension_compression::Host pure Unsupported-stub impls retired
// here. The wasmos-native EncodingHost / CompressionHost in
// extension_wasmos.rs return the same errors on the production
// path and are covered by the matching wasmos-side
// `_returns_unsupported` tests.

// Phase 6.2.l.2 — `impl extension_log_storage::Host::
// register_log_storage` retired here (no test in this crate
// exercised it). The wasmos-native `LogStorageHost` in
// extension_wasmos.rs still captures registrations on the
// production path.

// The `storage` interface lets a component register an ATTACH-able catalog
// backend (a DB scanner) in `load()`. Phase 2 (@5): the host records the
// (type-name -> extension) mapping in its own `storage_backends` registry
// (see `ExtensionManager` in `ducklink-host`) via the pending-storages
// drain path; the ATTACH intercept in `HostState::execute` looks the
// storage backend up by TYPE and routes to the owning component's
// `storage-dispatch` export. No C-API `duckdb_register_storage_extension`
// is involved -- see ADR Amendments A1 + B1/B2.
#[cfg(test)]
impl ExtensionStoreState {
    pub(crate) fn register_storage(
        &mut self,
        type_name: String,
        callback_handle: u32,
        // Phase 6.2.n Session 3 — mirror `Extopts` (same-shape as
        // storage.wit's re-export of types.wit's Extopts).
        options: Option<Extopts>,
    ) -> Result<u32, Duckerror> {
        let neutral_options = options.map(|o| reg::ExtOpts {
            description: o.description,
            tags: o.tags,
        });
        self.pending_storages.push(reg::StorageReg {
            extension: self.extension_name.clone(),
            type_name,
            callback_handle,
            options: neutral_options,
        });
        // The callback handle the component passed in is what the host will
        // pass back on every subsequent storage-dispatch call, so return it
        // unchanged (the @4 host-side API return-was-the-handle contract is
        // preserved for wire compatibility with the extension's expected
        // dispatch model).
        Ok(callback_handle)
    }
}

// Phase 6.2.l.2 — the three pure Unsupported-stub Host impls
// (extension_index::Host::register_index_type,
// extension_files_reg::Host::register_files,
// extension_collation::Host::register_collation) retired here.
// Nothing in the load path invokes wit-bindgen Host traits any
// more; the wasmos-native handlers in extension_wasmos.rs
// (IndexHost, FilesRegHost, CollationHost) return the same
// Unsupported error via the production install path, and the
// extension_wasmos test module covers each with a matching
// `_returns_unsupported` unit test. The interface WIT + docstring
// context stays with those handlers.

// Phase 6.2.l.2 — `impl extension_query::Host::query` retired
// here. No test in this crate exercised it; the wasmos-native
// `QueryHost` in extension_wasmos.rs forwards to the same
// `ExtensionServices::query` sink on the production dispatch path.

// The `nested-exec` interface: an EXECUTE-capable counterpart to `query` that
// runs SQL on a SIBLING connection to the same database. Guarded by a
// per-OS-thread nesting-depth counter so a fieldbook entry that recursively
// invokes `fieldbook_run` cannot spiral out of control. Forwards to the
// direction-specific `ExtensionServices::nested_exec` sink for the actual work.
#[cfg(test)]
impl ExtensionStoreState {
    /// Phase 6.2.n Session 2 — returns the neutral `NestedExecResult`
    /// mirror directly (was `extension_nested_exec::ExecResult` via
    /// `neutral_nestedresult_to_wit`). Both types have the same
    /// `rows` + `rows_affected` field shape, so test callers keep
    /// working without edits.
    fn nested_exec(&mut self, sql: String) -> Result<NestedExecResult, String> {
        let _depth = NestedExecDepthGuard::enter()?;
        self.services.nested_exec(&sql)
    }
}

// The `file-lock` interface: a host-provided advisory-lock primitive. WASI 0.2
// exposes no flock and cross-process serialization matters for caches / long
// downloads (the wasm-side counterpart to duckdb-cache's
// `store.rs::UriLock`). Implemented natively via fs2::FileExt::lock_exclusive,
// which resolves to fcntl(F_SETLKW) on Unix and LockFileEx on Windows.
//
// The acquired lock is stored as a [`LockHandleState`] inside the store's
// ResourceTable; the returned `Resource<LockHandle>` is the wit-bindgen handle
// that the guest holds. Dropping the wasm resource routes back through
// `HostLockHandle::drop` -> `ResourceTable::delete` -> `LockHandleState::drop`,
// which drops the `File` and releases the lock. The optional `release`
// method exists so a component can drop the lock early (after publishing,
// before slower cleanup) without waiting for the guest resource to fall
// out of scope.
//
// Path resolution: the guest passes an OS path string that resolves through
// its own WASI preopens (the ducklink CLI is expected to preopen the cache
// root under a name identical to its host path so guest + host see the same
// string). The host does no additional preopen validation -- see the WIT
// trust-model comment.
// Phase 6.2.n Session 2 — the test-only `acquire_exclusive` /
// `release` / `drop` inherent methods on ExtensionStoreState
// retired here alongside the `file_lock_host_acquire_and_release_
// round_trip` test that was their only caller. Both the
// production path (via wasmos FileLockHost) and its test coverage
// (via extension_wasmos::tests::file_lock_*) survive intact.

/// Native state backing a wasm `duckdb:extension/file-lock.lock-handle`
/// resource: an open `File` under an advisory `flock`. Dropping the value
/// closes the file and releases the lock; that is the invariant the WIT
/// resource trust model relies on.
struct LockHandleState {
    /// The lock-file handle. Held to keep the OS lock alive; never read from
    /// or written to.
    _file: std::fs::File,
    /// Diagnostic path (used only for future logging / debugging).
    _path: std::path::PathBuf,
}

impl LockHandleState {
    /// Open `path` (creating it if missing, never truncating -- matching
    /// duckdb-cache's `UriLock::acquire`) and take an exclusive advisory
    /// lock. Blocks until acquired.
    fn acquire_exclusive(path: &str) -> Result<Self, String> {
        use fs2::FileExt;
        let path_buf = std::path::PathBuf::from(path);
        if let Some(parent) = path_buf.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("file-lock: creating parent {}: {e}", parent.display()))?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path_buf)
            .map_err(|e| format!("file-lock: opening {}: {e}", path_buf.display()))?;
        file.lock_exclusive()
            .map_err(|e| format!("file-lock: flock {}: {e}", path_buf.display()))?;
        Ok(Self {
            _file: file,
            _path: path_buf,
        })
    }

    /// Non-blocking variant. Returns `Ok(None)` when the lock is held by
    /// another process, `Ok(Some(_))` on success, `Err` on IO failure.
    fn try_acquire_exclusive(path: &str) -> Result<Option<Self>, String> {
        use fs2::FileExt;
        let path_buf = std::path::PathBuf::from(path);
        if let Some(parent) = path_buf.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("file-lock: creating parent {}: {e}", parent.display()))?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path_buf)
            .map_err(|e| format!("file-lock: opening {}: {e}", path_buf.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                _path: path_buf,
            })),
            Err(err) => {
                // fs2 signals "would-block" with the OS-specific error kind
                // that maps to WouldBlock (EWOULDBLOCK/EAGAIN on Unix,
                // ERROR_LOCK_VIOLATION on Windows). Anything else is a real
                // IO failure.
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(format!(
                        "file-lock: try_flock {}: {err}",
                        path_buf.display()
                    ))
                }
            }
        }
    }
}

// Phase 6.2.n Session 2 — `neutral_nestedresult_to_wit` retired
// here alongside the wit-bindgen ExecResult return type on
// `state.nested_exec` (test-only inherent method). The 4
// nested_exec tests now use the neutral `NestedExecResult` mirror
// directly.

// ---------------------------------------------------------------------------
// Capture conversions (extension WIT -> neutral reg::*) + logging helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
fn convert_extension_funcargs(args: Vec<Funcarg>) -> Vec<reg::FuncArg> {
    args.into_iter()
        .map(|arg| reg::FuncArg {
            name: arg.name,
            logical: convert_extension_logicaltype(arg.logical),
        })
        .collect()
}

/// Phase 6.2.n Session 5 — takes the mirror `Logicaltype` instead
/// of wit-bindgen. Same 24 arms.
#[cfg(test)]
fn convert_extension_logicaltype(ty: Logicaltype) -> reg::LogicalType {
    match ty {
        Logicaltype::Boolean => reg::LogicalType::Boolean,
        Logicaltype::Int64 => reg::LogicalType::Int64,
        Logicaltype::Uint64 => reg::LogicalType::Uint64,
        Logicaltype::Float64 => reg::LogicalType::Float64,
        Logicaltype::Text => reg::LogicalType::Text,
        Logicaltype::Blob => reg::LogicalType::Blob,
        Logicaltype::Int32 => reg::LogicalType::Int32,
        Logicaltype::Timestamp => reg::LogicalType::Timestamp,
        Logicaltype::Int8 => reg::LogicalType::Int8,
        Logicaltype::Int16 => reg::LogicalType::Int16,
        Logicaltype::Uint8 => reg::LogicalType::Uint8,
        Logicaltype::Uint16 => reg::LogicalType::Uint16,
        Logicaltype::Uint32 => reg::LogicalType::Uint32,
        Logicaltype::Float32 => reg::LogicalType::Float32,
        Logicaltype::Date => reg::LogicalType::Date,
        Logicaltype::Time => reg::LogicalType::Time,
        Logicaltype::Timestamptz => reg::LogicalType::Timestamptz,
        Logicaltype::Decimal(shape) => reg::LogicalType::Decimal {
            width: shape.width,
            scale: shape.scale,
        },
        Logicaltype::Interval => reg::LogicalType::Interval,
        Logicaltype::Uuid => reg::LogicalType::Uuid,
        Logicaltype::Hugeint => reg::LogicalType::Hugeint,
        Logicaltype::Uhugeint => reg::LogicalType::UHugeint,
        Logicaltype::Complex(expr) => reg::LogicalType::Complex(expr),
    }
}

#[cfg(test)]
fn convert_extension_funcopts(opts: Funcopts) -> reg::FuncOpts {
    reg::FuncOpts {
        description: opts.description,
        tags: opts.tags,
        attributes: convert_extension_funcflags(opts.attributes),
    }
}

#[cfg(test)]
fn convert_extension_columndefs(columns: Vec<Columndef>) -> Vec<reg::ColumnDef> {
    columns
        .into_iter()
        .map(|col| reg::ColumnDef {
            name: col.name,
            logical: convert_extension_logicaltype(col.logical),
        })
        .collect()
}

#[cfg(test)]
fn convert_extension_funcflags(flags: Funcflags) -> reg::FuncFlags {
    reg::FuncFlags {
        deterministic: flags.contains(Funcflags::DETERMINISTIC),
        commutative: flags.contains(Funcflags::COMMUTATIVE),
        stateless: flags.contains(Funcflags::STATELESS),
        side_effecting: flags.contains(Funcflags::SIDEEFFECTING),
        deprecated: flags.contains(Funcflags::DEPRECATED),
    }
}

// Phase 6.2.l.2 — `log_scalar_registration`, `log_table_registration`,
// and `log_aggregate_registration` retired here. Only the deleted
// callback / registry Host impls used them.

pub fn summarize_runtime_funcargs(args: &[reg::FuncArg]) -> String {
    if args.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|arg| {
            let name = arg.name.as_ref().map(|s| s.as_str()).unwrap_or("-");
            format!("{name}:{}", describe_runtime_logicaltype(&arg.logical))
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

pub fn summarize_runtime_columns(columns: &[reg::ColumnDef]) -> String {
    if columns.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = columns
        .iter()
        .map(|col| {
            format!(
                "{}:{}",
                col.name,
                describe_runtime_logicaltype(&col.logical)
            )
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

pub fn summarize_funcopts(options: Option<&reg::FuncOpts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts.description.as_ref().map(|s| s.as_str()).unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            let attrs = opts.attributes.describe();
            format!("description='{description}', tags={tags}, attrs={attrs}")
        }
    }
}

pub fn summarize_extopts(options: Option<&reg::ExtOpts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts.description.as_ref().map(|s| s.as_str()).unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            format!("description='{description}', tags={tags}")
        }
    }
}

pub fn describe_runtime_logicaltype(ty: &reg::LogicalType) -> String {
    ty.describe()
}

// ---------------------------------------------------------------------------
// ExtensionInstance
// ---------------------------------------------------------------------------

/// A loaded extension component: its wasmtime store + raw component
/// instance. `dispatch_*` re-enter the guest's exports via the wasmos
/// sync export bridge (see Phase 6.2.i for the migration off wit-
/// bindgen typed dispatchers).
///
/// ADR-0029 Phase 6.2.j — struct trimmed after the Phase 6.2.i
/// export-migration completed. The former `bindings: DuckdbExtension`
/// field and the 14 lazily-built `xxx_bindings: Option<...>` fields
/// (storage, index, files, parser, optimizer, copy, secret,
/// storage_write, table_stream, aggregate_incr, conn, file_write,
/// index_write, settings, log_storage, arrow_ext) all retired here —
/// every dispatch now flows through
/// `wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export`
/// using the raw `instance`, no typed dispatcher struct required.
///
/// Phase 6.2.k retired the last of those wrapper modules whose only
/// remaining role was per-world type re-exports. `FileInfo`,
/// `TableOpenResult`, `CopyFromBindResult`, `LogEntry`, `IndexHit`,
/// `FilterOp`, `TableFilter`, `SecretKv` now live as plain Rust
/// mirror structs on `crate::extension` directly — no wit-bindgen
/// dependency in ducklink-runtime's public SPI record surface.
pub struct ExtensionInstance {
    store: Store<ExtensionStoreState>,
    instance: wasmtime::component::Instance,
}


/// ADR-0029 Phase 6.2.i — decode a `wasmos_runtime_api::Value` in the
/// shape emitted by a guest `duckerror` variant return back to the
/// wit-bindgen `Duckerror` enum. Used by the
/// `dispatch_*` methods in `ExtensionInstance` that migrated off
/// wit-bindgen typed dispatchers via `sync_export_bridge::call_export`;
/// their public API returns `Result<T, Duckerror>`
/// for backward compat, so the bridge's `Value::Result(Err(payload))`
/// gets decoded here.
///
/// WIT arms (from `wit/duckdb-extension/types.wit`):
///
/// ```text
/// variant duckerror {
///   invalidargument(string),
///   unsupported(string),
///   invalidstate(string),
///   io(string),
///   internal(string),
/// }
/// ```
///
/// Unknown discriminants or missing payloads fall through to
/// `Duckerror::Internal("...")` so a shape mismatch surfaces with a
/// diagnostic rather than a panic.
// ADR-0029 Phase 6.2.i.7 — duckerror_from_value + export_result_to_
// duckerror moved to `crate::export_marshal` alongside the full
// Duckvalue + record marshalling suite. Callsites reach both via
// the module-level `use crate::export_marshal::*` import.
pub(crate) use crate::export_marshal::export_result_to_duckerror;


/// M2b: the storage interface's scan types (`scan-request` /
/// `scan-filter` / `compare-op`) used when driving a pushdown scan
/// into the component.
///
/// ADR-0029 Phase 6.2.l' — these used to re-export the wit-bindgen-
/// generated records from the `duckdb_extension_storage_bindings`
/// wrapper. That wrapper existed SOLELY to generate a distinct
/// `Duckvalue` per world (wit-bindgen emits one Rust enum per
/// bindings module for the same WIT type), which then required a
/// bridge (`storage_duckvalue_to_ext`) to convert into the base
/// `Duckvalue` at the callsite. The mirror below
/// uses `Duckvalue` directly — no per-world clone,
/// no bridge, no wrapper needed.
pub mod storage_scan {
    /// Mirror of WIT `storage.compare-op`. Comparison pushed into a
    /// scan as an AND-ed predicate.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CompareOp {
        Eq,
        Ne,
        Lt,
        Le,
        Gt,
        Ge,
        IsNull,
        IsNotNull,
    }

    /// Mirror of WIT `storage.scan-filter`. One predicate. `column`
    /// indexes the table's FULL column list (not the projection).
    /// Pushdown is best-effort: the engine re-applies every filter,
    /// so a backend may ignore any predicate it cannot evaluate.
    /// `value` is ignored for `IsNull` / `IsNotNull`.
    #[derive(Clone, Debug)]
    pub struct ScanFilter {
        pub column: u32,
        pub op: CompareOp,
        pub value: super::Duckvalue,
    }

    /// Mirror of WIT `storage.scan-request`. What the engine asks a
    /// scan to produce.
    #[derive(Clone, Debug)]
    pub struct ScanRequest {
        pub table: String,
        /// Column indices to emit, in order; empty = all columns.
        pub projection: Vec<u32>,
        pub filters: Vec<ScanFilter>,
        /// Best-effort row cap.
        pub limit: Option<u64>,
    }
}

// ADR-0029 Phase 6.2.m — base-wrapper record/enum mirrors.
//
// The `duckdb_extension_bindings` wrapper (the last surviving
// wit-bindgen wrapper after Phase 6.2.l') still supplies the
// WIT-generated workhorse types (`Duckvalue`, `Columndef`,
// `Duckerror`, `Logicaltype`, `Colvec`, `Column`, `Resultset`,
// ...). Consumers that name any of these transitively depend on
// the wit-bindgen module tree. Phase 6.2.m mirrors each with a
// plain Rust type on `crate::extension`, wire-compatible with the
// WIT record, and provides `From`/`Into` converters so the wasmos-
// native handlers in `extension_wasmos.rs` (which reference the
// wit-bindgen types in their `#[host_iface]`-generated method
// signatures) keep working while the public API of ducklink-
// runtime returns the mirrors.
//
// Session 1: `Duckerror` (5 arms).

/// SPI error type: the union of failure modes any `duckdb:extension/
/// *` host import or dispatch can return. Mirror of WIT
/// `types.duckerror` — same 5 arms, same string payload semantics.
/// Constructed by ducklink-runtime code + returned across the
/// public API so consumers (ducklink-host, external extension
/// hosts) don't need to depend on the wit-bindgen module tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Duckerror {
    /// The caller passed an invalid argument shape or value.
    Invalidargument(String),
    /// The requested capability isn't offered by this build (the
    /// underlying DuckDB stable C API has no equivalent hook).
    Unsupported(String),
    /// The host or extension is in a state where the requested
    /// operation cannot proceed (e.g. unknown handle, re-entrant
    /// call in a non-re-entrant context).
    Invalidstate(String),
    /// The operation hit an I/O error the caller can retry.
    Io(String),
    /// A failure not fitting any other arm — the message carries
    /// diagnostic detail.
    Internal(String),
}



impl std::fmt::Display for Duckerror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Duckerror::Invalidargument(m) => write!(f, "invalid-argument: {m}"),
            Duckerror::Unsupported(m) => write!(f, "unsupported: {m}"),
            Duckerror::Invalidstate(m) => write!(f, "invalid-state: {m}"),
            Duckerror::Io(m) => write!(f, "io: {m}"),
            Duckerror::Internal(m) => write!(f, "internal: {m}"),
        }
    }
}

impl std::error::Error for Duckerror {}

// ─── Phase 6.2.m Session 2 — Duckvalue family mirrors ────────────

/// DuckDB DECIMAL: HUGEINT-backed scaled integer. Mirror of WIT
/// `types.decimalvalue`. `value = ((upper as i128) << 64 | lower as
/// i128)`, interpreted with `width` total digits and `scale`
/// fractional digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decimalvalue {
    pub lower: u64,
    pub upper: u64,
    pub width: u8,
    pub scale: u8,
}

/// 128-bit signed integer value. Mirror of WIT `types.hugeintvalue`.
/// WIT has no native s128 so the runtime reassembles the value via
/// `((upper as i128) << 64 | lower as i128)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hugeintvalue {
    pub lower: u64,
    pub upper: i64,
}

/// 128-bit unsigned integer value. Mirror of WIT `types.uhugeintvalue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uhugeintvalue {
    pub lower: u64,
    pub upper: u64,
}

/// DuckDB INTERVAL: months + days + microseconds components.
/// Mirror of WIT `types.intervalvalue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intervalvalue {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

/// 128-bit UUID. `hi`/`lo` are the logical big-endian halves of the
/// UUID value (NOT the sign-flipped physical hugeint storage DuckDB
/// uses internally). Mirror of WIT `types.uuidvalue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uuidvalue {
    pub hi: u64,
    pub lo: u64,
}

/// ESCAPE-HATCH composite value. `type_expr` is a DuckDB type string
/// (e.g. `"INTEGER[]"` / `"STRUCT(a INTEGER, b VARCHAR)"`) and `json`
/// is the value rendered as JSON. Flat record — does NOT reference
/// `Duckvalue`, so there's no recursive cycle. Mirror of WIT
/// `types.complexvalue`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Complexvalue {
    pub type_expr: String,
    pub json: String,
}

/// The row-major SPI data value. Mirror of WIT `types.duckvalue` —
/// 25 arms covering scalar, temporal, decimal, hugeint, uuid,
/// interval, and the `complex` JSON escape hatch for nested
/// LIST/STRUCT/MAP/ARRAY values (structural nested value transport
/// lives on the columnar `Column` variant).
#[derive(Clone, Debug, PartialEq)]
pub enum Duckvalue {
    Null,
    Boolean(bool),
    Int64(i64),
    Uint64(u64),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
    Int32(i32),
    Timestamp(i64),
    Int8(i8),
    Int16(i16),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Float32(f32),
    Date(i32),
    Time(i64),
    Timestamptz(i64),
    Decimal(Decimalvalue),
    Interval(Intervalvalue),
    Uuid(Uuidvalue),
    Hugeint(Hugeintvalue),
    Uhugeint(Uhugeintvalue),
    Complex(Complexvalue),
}



/// DECIMAL width/scale shape. Mirror of WIT `types.decimalshape`.
/// Carried on `Logicaltype::Decimal` so DECIMAL type-shape is
/// structural.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decimalshape {
    pub width: u8,
    pub scale: u8,
}

/// The logical (schema-side) type of a column or scalar. Mirror of
/// WIT `types.logicaltype`. 24 arms: 22 fieldless scalar / temporal
/// types + `Decimal(Decimalshape)` for structural DECIMAL(width,
/// scale) + `Complex(String)` escape hatch carrying a DuckDB type-
/// expression (e.g. `"INTEGER[]"`, `"STRUCT(a INT, b VARCHAR)"`) for
/// nested LIST/STRUCT/MAP/ARRAY types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Logicaltype {
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
    Decimal(Decimalshape),
    Interval,
    Uuid,
    Hugeint,
    Uhugeint,
    Complex(String),
}

/// Column definition: `(name, logical type)`. Mirror of WIT
/// `types.columndef`. Used across storage / table / arrow / copy
/// schemas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Columndef {
    pub name: String,
    pub logical: Logicaltype,
}




// ─── Column-types mirrors ────────────────────────────────────────

/// 128-bit signed integer scalar for the columnar `Column::Hugeint`
/// arm. Mirror of WIT `column-types.duck-int128`. Layout parallels
/// [`Hugeintvalue`] (lower `u64` + upper `i64`); named distinctly
/// because wit-bindgen emits a separate Rust type per WIT record
/// declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuckInt128 {
    pub lower: u64,
    pub upper: i64,
}

/// 128-bit unsigned integer scalar for `Column::Uhugeint`. Mirror of
/// WIT `column-types.duck-uint128`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuckUint128 {
    pub lower: u64,
    pub upper: u64,
}

/// Opaque byte-encoded nested column payload for `Column::ListCol`
/// and `Column::StructCol`. Mirror of WIT `column-types.nested-
/// column`. The runtime-defined compact binary encoding is owned by
/// this crate's marshalling layer; consumers of the WIT boundary
/// treat it as bytes.
#[derive(Clone, Debug)]
pub struct NestedColumn {
    pub encoded: Vec<u8>,
}

/// Opaque byte-encoded map column payload for `Column::MapCol`.
/// Mirror of WIT `column-types.map-column`. Keys + values ride
/// separate byte buffers.
#[derive(Clone, Debug)]
pub struct MapColumn {
    pub keys_encoded: Vec<u8>,
    pub vals_encoded: Vec<u8>,
}

/// Opaque byte-encoded array column payload for `Column::ArrayCol`.
/// Mirror of WIT `column-types.array-column`. `size` is the ARRAY[N]
/// element count.
#[derive(Clone, Debug)]
pub struct ArrayColumn {
    pub size: u32,
    pub encoded: Vec<u8>,
}

/// A typed, contiguous column buffer — one WIT-defined memcpy shape
/// per DuckDB physical type. Mirror of WIT `column-types.column`.
/// 27 arms:
/// * Fixed-width numeric / temporal (boolean, i8/16/32/64, u8/16/32/
///   64, f32/64, date, time, timestamp, timestamptz) — bulk memcpy.
/// * Record-payload (decimal, interval, uuid, hugeint, uhugeint) —
///   bulk memcpy.
/// * Var-width (text, blob) — element-wise (unavoidable).
/// * Nested S1 (list-col, struct-col, map-col, array-col) — one
///   opaque byte payload per vector.
/// * Escape hatch (complex) — element-wise `Complexvalue` list.
#[derive(Clone, Debug)]
pub enum Column {
    Boolean(Vec<bool>),
    Int64(Vec<i64>),
    Uint64(Vec<u64>),
    Float64(Vec<f64>),
    Int32(Vec<i32>),
    Timestamp(Vec<i64>),
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Uint8(Vec<u8>),
    Uint16(Vec<u16>),
    Uint32(Vec<u32>),
    Float32(Vec<f32>),
    Date(Vec<i32>),
    Time(Vec<i64>),
    Timestamptz(Vec<i64>),
    Decimal(Vec<Decimalvalue>),
    Interval(Vec<Intervalvalue>),
    Uuid(Vec<Uuidvalue>),
    Text(Vec<String>),
    Blob(Vec<Vec<u8>>),
    Hugeint(Vec<DuckInt128>),
    Uhugeint(Vec<DuckUint128>),
    ListCol(NestedColumn),
    StructCol(NestedColumn),
    MapCol(MapColumn),
    ArrayCol(ArrayColumn),
    Complex(Vec<Complexvalue>),
}

/// A column plus its validity + row count — the unit of columnar
/// transfer. Mirror of WIT `column-types.colvec`. `validity` is a
/// packed little-endian bitmap (bit i set => row i valid); an EMPTY
/// bitmap means "all rows valid" (zero-alloc fast path).
#[derive(Clone, Debug)]
pub struct Colvec {
    pub data: Column,
    pub validity: Vec<u8>,
    pub rows: u32,
}


// Column-types re-declares decimalvalue, intervalvalue, uuidvalue,
// and complexvalue with identical layout to their types.wit
// counterparts. wit-bindgen generates distinct Rust types per WIT
// declaration, so we bridge column-types' variants through the
// unified mirrors above.



// ─── Phase 6.2.m Session 6 — flags mirrors ───────────────────────

/// Function-registration flags. Mirror of WIT `types.funcflags` (a
/// WIT `flags` value, marshalled as a bitset). Same associated
/// constants + `.contains()` / `.empty()` / `|` API as the wit-
/// bindgen bitflags shape.
///
/// Kept hand-rolled (rather than routed through the `bitflags`
/// crate) to avoid pulling in a new dependency for one 5-flag type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Funcflags(u32);

impl Funcflags {
    pub const DETERMINISTIC: Self = Self(0b0_0001);
    pub const COMMUTATIVE: Self = Self(0b0_0010);
    pub const STATELESS: Self = Self(0b0_0100);
    pub const SIDEEFFECTING: Self = Self(0b0_1000);
    pub const DEPRECATED: Self = Self(0b1_0000);

    /// Zero-flag value — the identity element of `|`.
    pub const fn empty() -> Self { Self(0) }

    /// Whether `self` has every flag `other` sets.
    pub const fn contains(&self, other: Self) -> bool { self.0 & other.0 == other.0 }
}

impl std::ops::BitOr for Funcflags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

impl std::ops::BitOrAssign for Funcflags {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}


/// Connection lifecycle event bits. Mirror of WIT
/// `lifecycle.conn-events` (2-flag bitset: opened + closed).
///
/// The wasmos-native `LifecycleHost` in `crate::extension_wasmos`
/// carries its own `#[derive(WitFlags)]`-backed `ConnEvents` for the
/// wire dispatch path; this mirror lives on the public API so
/// consumers reach one flags type consistently, and pairs with
/// bidirectional `From` converters to the wit-bindgen shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnEvents(u32);

impl ConnEvents {
    pub const OPENED: Self = Self(0b01);
    pub const CLOSED: Self = Self(0b10);

    pub const fn empty() -> Self { Self(0) }
    pub const fn contains(&self, other: Self) -> bool { self.0 & other.0 == other.0 }
}

impl std::ops::BitOr for ConnEvents {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl std::ops::BitOrAssign for ConnEvents {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}

// ─── Session 5 mirrors — smaller records / enums ─────────────────

/// Config-lookup error surface. Mirror of WIT `types.configerror`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Configerror {
    Invalidkey(String),
    Typemismatch(String),
    Unavailable(String),
    Internalconfig(String),
}

/// Log severity. Mirror of WIT `types.loglevel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loglevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Structured log field. Mirror of WIT `types.logfield`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logfield {
    pub key: String,
    pub value: String,
}

/// Per-invocation context passed alongside scalar callback args.
/// Mirror of WIT `types.invokeinfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invokeinfo {
    pub rowindex: Option<u64>,
    pub iswindow: bool,
}

/// The kinds of capability a runtime exposes. Mirror of WIT
/// `types.capabilitykind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capabilitykind {
    Scalar,
    Table,
    Aggregate,
    Pragma,
    Macro,
    Catalog,
    FileFormat,
}

/// A function argument declaration: optional name + logical type.
/// Mirror of WIT `types.funcarg`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funcarg {
    pub name: Option<String>,
    pub logical: Logicaltype,
}

/// Metadata carried alongside a scalar / table / aggregate
/// registration: description + tags. Mirror of WIT `types.extopts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extopts {
    pub description: Option<String>,
    pub tags: Vec<String>,
}

/// Function-registration options: description + tags + attributes
/// (a [`Funcflags`] bitset). Mirror of WIT `types.funcopts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funcopts {
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub attributes: Funcflags,
}

/// `Loadresult` — what a component's `load` export returns. Mirror
/// of WIT `types.loadresult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loadresult {
    pub name: String,
    pub version: Option<String>,
    pub requires: Vec<Capabilitykind>,
}









/// A resultset returned by a scan / query dispatch: row-major
/// list of rows, each a list of `Duckvalue`s. Mirror of WIT
/// `types.resultset` (which is `list<list<duckvalue>>`).
pub type Resultset = Vec<Vec<Duckvalue>>;

/// A batch of input rows delivered to a table function or scalar
/// batch dispatch. Same wire shape as [`Resultset`] — mirror of WIT
/// `types.rowbatch`.
pub type Rowbatch = Vec<Vec<Duckvalue>>;


// ADR-0029 Phase 6.2.k — SPI record/enum mirrors.
//
// These types used to be re-exports of the wit-bindgen-generated
// records inside per-world binding wrappers (`duckdb_extension_index_
// bindings::exports::duckdb::extension::index_dispatch::IndexHit`,
// etc.). Phase 6.2.i migrated every dispatch off wit-bindgen through
// `sync_export_bridge::call_export` + `crate::export_marshal`; Phase
// 6.2.j retired the nine wrapper modules whose dispatchers no caller
// touched. What kept the last seven wrappers alive was these type
// re-exports: they leaked wit-bindgen's generated Rust shape onto
// ducklink-runtime's public API, so `ducklink-host` and any external
// consumer that named `ducklink_runtime::extension::IndexHit`
// depended transitively on the wit-bindgen module tree.
//
// Phase 6.2.k defines each record/enum as a plain Rust struct here
// — same fields, same wire semantics, no wit-bindgen coupling. The
// seven wrappers can then retire in the same pass. The types stay
// wire-shaped (matching the WIT definitions in
// `wit/deps/duckdb-extension/{index,copy,secret,table-stream,file-
// write,log-storage}-dispatch.wit`) so `export_marshal` decoders
// build them directly from `wasmos_runtime_api::Value` payloads
// with no downstream lift/lower needed.

/// M2a (index): one hit returned by an ANN or general index scan —
/// a `(rowid, distance)` pair. Mirror of WIT
/// `index-dispatch.index-hit`. Kept on the public API so the host
/// surfaces hits up through the index-host import.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexHit {
    pub rowid: i64,
    pub distance: f32,
}

/// 2.1.0 (Item 1): result of binding a COPY FROM reader (reader
/// handle + projected column schema). Mirror of WIT
/// `copy-dispatch.copy-from-bind-result`. `columns` is the base
/// `Columndef` — the WIT wire type is the SAME
/// record, not a per-world clone.
#[derive(Clone, Debug)]
pub struct CopyFromBindResult {
    pub reader: u32,
    pub columns: Vec<Columndef>,
}

/// 2.1.0 (Item 2): one flat key=value entry of a materialized secret.
/// Mirror of WIT `secret-dispatch.secret-kv`.
#[derive(Clone, Debug, PartialEq)]
pub struct SecretKv {
    pub key: String,
    pub value: String,
}

/// 2.2.0 (Item 6): result of opening a streaming table cursor
/// (cursor handle + projected column schema). Mirror of WIT
/// `table-stream-dispatch.table-open-result`.
#[derive(Clone, Debug)]
pub struct TableOpenResult {
    pub cursor: u32,
    pub columns: Vec<Columndef>,
}

/// 3.1.0: comparator for a pushed-down filter. Mirror of WIT
/// `table-stream-dispatch.filter-op`. The `IsIn` variant carries no
/// payload — the flattened IN-list rides on the enclosing
/// [`TableFilter::values`] vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// The column value is one of `values` (a flattened IN-list).
    IsIn,
    IsNull,
    IsNotNull,
}

/// 3.1.0: the neutral, by-value-safe pushed-down filter descriptor.
/// Mirror of WIT `table-stream-dispatch.table-filter`. The
/// core<->host bridge builds the conjunctive filter set that the
/// streaming `TableFunction` pushes to `call-table-open-filtered`.
#[derive(Clone, Debug)]
pub struct TableFilter {
    pub column: u32,
    pub op: FilterOp,
    pub values: Vec<Duckvalue>,
}

/// 2.2.0 (Item 7): metadata for one path returned by
/// `file-write-dispatch.file-stat`. Mirror of WIT
/// `file-write-dispatch.file-info`.
#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub is_directory: bool,
}

/// 3.2.0: one log record crossing the WIT boundary into a registered
/// log sink. Mirror of WIT `log-storage-dispatch.log-entry`. Class B
/// parity with the stable `duckdb_register_log_storage` C API — the
/// direction-specific sink (the C API installer in `ducklink-
/// extension/src/reg_duckdb.rs`) constructs entries by-value.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub level: u32,
    pub message: String,
    pub tags: Option<Vec<(String, String)>>,
    pub ts_micros: i64,
}


impl ExtensionInstance {
    /// Phase 6.2.j — trimmed constructor. No wit-bindgen typed
    /// dispatchers to stash any more; every `dispatch_*` re-enters
    /// the guest via `sync_export_bridge::call_export` using
    /// `instance` directly.
    pub fn new(
        store: Store<ExtensionStoreState>,
        instance: wasmtime::component::Instance,
    ) -> Self {
        Self { store, instance }
    }

    pub fn dispatch_scalar(
        &mut self,
        dispatcher_handle: u32,
        args: &[Duckvalue],
        ctx: Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {
        // Phase 6.2.i.7 — migrated. Invokeinfo record: rowindex
        // (option<u64>), iswindow (bool).
        use crate::export_marshal::*;
        let ctx_val = wasmos_runtime_api::Value::Record(vec![
            (
                "rowindex".into(),
                wasmos_runtime_api::Value::Option(
                    ctx.rowindex
                        .map(|n| Box::new(wasmos_runtime_api::Value::U64(n))),
                ),
            ),
            ("iswindow".into(), wasmos_runtime_api::Value::Bool(ctx.iswindow)),
        ]);
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/callback-dispatch@5.0.0"),
            "call-scalar",
            &[
                wasmos_runtime_api::Value::U32(dispatcher_handle),
                duckvalue_list_to_value(args),
                ctx_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-scalar dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-scalar", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-scalar: expected Ok(duckvalue), got None".into()))?;
            value_to_duckvalue(v)
        })
    }

    #[allow(clippy::ptr_arg)] // the bindgen call takes &Vec (the rowbatch type), not a slice
    pub fn dispatch_scalar_batch(
        &mut self,
        dispatcher_handle: u32,
        rows: &Vec<Vec<Duckvalue>>,
        ctx: Invokeinfo,
    ) -> Result<Vec<Duckvalue>, Duckerror> {
        // Phase 6.2.i.7 — migrated. Row-major → columnar pivot in
        // Rust (via existing rows_to_colvecs helper), marshal Colvec
        // via export_marshal, dispatch, lift Colvec return + pivot
        // back to rows via colvec_to_values.
        let args = rows_to_colvecs(rows);
        self.dispatch_scalar_batch_col(dispatcher_handle, &args, ctx)
            .map(colvec_to_values)
    }

    /// Column-native scalar batch dispatch.
    pub fn dispatch_scalar_batch_col(
        &mut self,
        dispatcher_handle: u32,
        args: &[Colvec],
        ctx: Invokeinfo,
    ) -> Result<Colvec, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let ctx_val = wasmos_runtime_api::Value::Record(vec![
            (
                "rowindex".into(),
                wasmos_runtime_api::Value::Option(
                    ctx.rowindex.map(|n| Box::new(wasmos_runtime_api::Value::U64(n))),
                ),
            ),
            ("iswindow".into(), wasmos_runtime_api::Value::Bool(ctx.iswindow)),
        ]);
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/callback-dispatch@5.0.0"),
            "call-scalar-batch-col",
            &[
                wasmos_runtime_api::Value::U32(dispatcher_handle),
                colvec_list_to_value(args),
                ctx_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-scalar-batch-col dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-scalar-batch-col", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-scalar-batch-col: expected Ok(colvec), got None".into()))?;
            value_to_colvec(v)
        })
    }

    pub fn dispatch_table(
        &mut self,
        dispatcher_handle: u32,
        args: &[Duckvalue],
    ) -> Result<Resultset, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/callback-dispatch@5.0.0"),
            "call-table",
            &[
                wasmos_runtime_api::Value::U32(dispatcher_handle),
                duckvalue_list_to_value(args),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-table dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-table", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-table: expected Ok(resultset), got None".into()))?;
            value_to_resultset(v)
        })
    }

    pub fn dispatch_aggregate(
        &mut self,
        dispatcher_handle: u32,
        rows: &Rowbatch,
    ) -> Result<Duckvalue, Duckerror> {
        // Phase 6.2.i.7 — delegate via the col variant.
        let args = rows_to_colvecs(rows);
        self.dispatch_aggregate_col(dispatcher_handle, &args)
    }

    /// Column-native aggregate dispatch.
    pub fn dispatch_aggregate_col(
        &mut self,
        dispatcher_handle: u32,
        args: &[Colvec],
    ) -> Result<Duckvalue, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/callback-dispatch@5.0.0"),
            "call-aggregate-col",
            &[
                wasmos_runtime_api::Value::U32(dispatcher_handle),
                colvec_list_to_value(args),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-aggregate-col dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-aggregate-col", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-aggregate-col: expected Ok(duckvalue), got None".into()))?;
            value_to_duckvalue(v)
        })
    }

    pub fn dispatch_pragma(
        &mut self,
        dispatcher_handle: u32,
        args: &[Duckvalue],
    ) -> Result<Option<Duckvalue>, Duckerror> {
        // Phase 6.2.i.7 — migrated. Returns result<option<duckvalue>,
        // duckerror>.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/callback-dispatch@5.0.0"),
            "call-pragma",
            &[
                wasmos_runtime_api::Value::U32(dispatcher_handle),
                duckvalue_list_to_value(args),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-pragma dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-pragma", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-pragma: expected Ok(option<duckvalue>), got None".into()))?;
            value_to_optional_duckvalue(v)
        })
    }

    pub fn dispatch_cast(
        &mut self,
        dispatcher_handle: u32,
        value: &Duckvalue,
    ) -> Result<Duckvalue, Duckerror> {
        // Phase 6.2.i.7 — migrated. Single-value → 1-row colvec via
        // existing column_from_values helper; marshal Colvec via
        // export_marshal; lift return Colvec + extract first row's
        // value via colvec_to_values.
        use crate::export_marshal::*;
        let arg = column_from_values(&[value]);
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/callback-dispatch@5.0.0"),
            "call-cast-col",
            &[
                wasmos_runtime_api::Value::U32(dispatcher_handle),
                colvec_to_value(&arg),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-cast-col dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-cast-col", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-cast-col: expected Ok(colvec), got None".into()))?;
            let c = value_to_colvec(v)?;
            Ok(colvec_to_values(c)
                .into_iter()
                .next()
                .unwrap_or(Duckvalue::Null))
        })
    }

    pub fn drain_pending(&mut self) -> PendingRegistrationsData {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).drain_pending() }
    }

    /// Drive the component's `guest.shutdown` export. The C API installer in
    /// `ducklink-extension/src/reg_duckdb.rs` calls this on Drop so a component
    /// that owns external resources (a background writer, an open file handle,
    /// a network client) can flush + release them before the wasmtime store
    /// tears down. Mirrors the `bindings.duckdb_extension_guest().call_load`
    /// shape used at load time (see `load_component`).
    pub fn dispatch_shutdown(&mut self) -> Result<bool, Duckerror> {
        // ADR-0029 Phase 6.2.i.4 — migrated from
        // `bindings.duckdb_extension_guest().call_shutdown(store)` to
        // the wasmos sync_export_bridge. Same interface + method
        // names + wire semantics; the Ok payload is `bool` and the
        // Err payload is `duckerror` (decoded via export_result_to_
        // duckerror + duckerror_from_value).
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/guest@5.0.0"),
            "shutdown",
            &[],
        )
        .map_err(|e| Duckerror::Internal(format!("shutdown dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "shutdown", |payload| match payload {
            Some(wasmos_runtime_api::Value::Bool(b)) => Ok(*b),
            Some(other) => Err(Duckerror::Internal(format!(
                "shutdown: expected Ok(bool), got {other:?}"
            ))),
            None => Err(Duckerror::Internal(
                "shutdown: expected Ok(bool), got Ok(None)".to_string(),
            )),
        })
    }

    /// Drive the component's `guest.reconfigure` export. The C API installer
    /// forwards `SET`-triggered option changes here (the `keys` list is the set
    /// of option names whose values just changed) so the component can refresh
    /// any cached derived state before the next dispatch. Mirrors the
    /// `bindings.duckdb_extension_guest().call_load` shape used at load time.
    pub fn dispatch_reconfigure(
        &mut self,
        keys: &[String],
    ) -> Result<bool, Duckerror> {
        // ADR-0029 Phase 6.2.i.4 — sibling of dispatch_shutdown.
        // `keys` list<string> lowers to Value::List of Value::String.
        let keys_val = wasmos_runtime_api::Value::List(
            keys.iter().map(|k| wasmos_runtime_api::Value::String(k.clone())).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/guest@5.0.0"),
            "reconfigure",
            &[keys_val],
        )
        .map_err(|e| Duckerror::Internal(format!("reconfigure dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "reconfigure", |payload| match payload {
            Some(wasmos_runtime_api::Value::Bool(b)) => Ok(*b),
            Some(other) => Err(Duckerror::Internal(format!(
                "reconfigure: expected Ok(bool), got {other:?}"
            ))),
            None => Err(Duckerror::Internal(
                "reconfigure: expected Ok(bool), got Ok(None)".to_string(),
            )),
        })
    }

    /// Drains only the captured storage-backend registrations (see
    /// `ExtensionStoreState::take_pending_storages`).
    pub fn take_pending_storages(&mut self) -> Vec<crate::reg::StorageReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_storages() }
    }

    /// Item 3 / M2a: drains the captured custom-index TYPE registrations (see
    /// `ExtensionStoreState::take_pending_indexes`).
    pub fn take_pending_indexes(&mut self) -> Vec<crate::reg::IndexReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_indexes() }
    }

    /// httpfs M2: drains the captured files-backend registrations (see
    /// `ExtensionStoreState::take_pending_files`).
    pub fn take_pending_files(&mut self) -> Vec<crate::reg::FilesReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_files() }
    }

    /// Item 2: drains the captured collation registrations (see
    /// `ExtensionStoreState::take_pending_collations`).
    pub fn take_pending_collations(&mut self) -> Vec<crate::reg::CollationReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_collations() }
    }

    /// Item 4: drains the captured pragma registrations (see
    /// `ExtensionStoreState::take_pending_pragmas`).
    pub fn take_pending_pragmas(&mut self) -> Vec<crate::reg::PragmaReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_pragmas() }
    }

    // --- 2.1.0 additive drains (mirror take_pending_pragmas) ---

    /// 2.1.0 (Item 1): drains the captured COPY-handler registrations.
    pub fn take_pending_copy_handlers(&mut self) -> Vec<crate::reg::CopyHandlerReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_copy_handlers() }
    }

    /// 2.1.0 (Item 2): drains the captured secret type/provider registrations.
    pub fn take_pending_secrets(&mut self) -> Vec<crate::reg::SecretReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_secrets() }
    }

    /// 2.1.0 (Item 3): drains the captured option/settings registrations.
    pub fn take_pending_settings(&mut self) -> Vec<crate::reg::SettingReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_settings() }
    }

    /// 2.1.0 (Item 5): drains the captured table-macro registrations.
    pub fn take_pending_table_macros(&mut self) -> Vec<crate::reg::TableMacroReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_table_macros() }
    }

    /// 2.1.0 (Item 5): drains the captured modified-logical-type registrations.
    pub fn take_pending_modified_types(&mut self) -> Vec<crate::reg::ModifiedTypeReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_modified_types() }
    }

    /// 2.1.0 (Item 5): drains the captured ENUM-type registrations.
    pub fn take_pending_enum_types(&mut self) -> Vec<crate::reg::EnumTypeReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_enum_types() }
    }

    // --- 2.2.0 additive drains (Items 6-7; mirror the 2.1.0 drains) ---

    /// 2.2.0 (Item 6): drains the captured richer scalar (scalar-ex) registrations.
    pub fn take_pending_scalar_ex(&mut self) -> Vec<crate::reg::ScalarExReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_scalar_ex() }
    }

    /// 2.2.0 (Item 7): drains the captured connection-lifecycle subscriptions.
    pub fn take_pending_conn_callbacks(&mut self) -> Vec<crate::reg::ConnCallbackReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_conn_callbacks() }
    }

    /// 2.2.0 (Item 7): drains the captured coordinate-system (CRS) registrations.
    pub fn take_pending_coordinate_systems(&mut self) -> Vec<crate::reg::CoordinateSystemReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_coordinate_systems() }
    }

    /// 2.2.0 (Item 7): drains the captured Arrow-table-producer registrations.
    pub fn take_pending_arrow_tables(&mut self) -> Vec<crate::reg::ArrowTableReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_arrow_tables() }
    }

    /// 2.2.0 (Item 7): drains the captured text-encoding registrations.
    pub fn take_pending_encodings(&mut self) -> Vec<crate::reg::EncodingReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_encodings() }
    }

    /// 2.2.0 (Item 7): drains the captured compression-codec registrations.
    pub fn take_pending_compressions(&mut self) -> Vec<crate::reg::CompressionReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_compressions() }
    }

    /// 2.3.0 / v3: drains the captured parser-extension registrations. The core
    /// shim wires each into a DuckDB `ParserExtension`.
    pub fn take_pending_parsers(&mut self) -> Vec<crate::reg::ParserReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_parsers() }
    }

    /// 2.3.0 / v3: drains the captured optimizer-rule registrations. The core shim
    /// wires each into a DuckDB `OptimizerExtension`.
    pub fn take_pending_optimizers(&mut self) -> Vec<crate::reg::OptimizerReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_optimizers() }
    }

    /// 3.1.0: drains the captured streaming/filter-pushdown table-fn registrations
    /// (the first additive MINOR off the frozen major-3 baseline). The core shim
    /// wires each into a C++ streaming `TableFunction` with `filter_pushdown = true`
    /// that drives the component's `table-stream-dispatch.call-table-open-filtered`.
    pub fn take_pending_filterable_tables(&mut self) -> Vec<crate::reg::FilterableTableReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_filterable_tables() }
    }

    // --- 2.1.0 (Item 1): copy-dispatch re-entry ---
    // Drives a registered COPY handler's exported `copy-dispatch`. Types are
    // remapped to the base `extension_types` (see lib.rs `with`), so no per-world
    // conversion is needed.

    /// COPY TO: bind a writer for `path`; returns a writer handle.
    pub fn copy_to_bind(
        &mut self,
        handle: u32,
        path: &str,
        columns: &[Columndef],
        options: &[(String, String)],
    ) -> Result<u32, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let opts_val = wasmos_runtime_api::Value::List(
            options.iter().map(|(k, v)| wasmos_runtime_api::Value::Tuple(vec![
                wasmos_runtime_api::Value::String(k.clone()),
                wasmos_runtime_api::Value::String(v.clone()),
            ])).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/copy-dispatch@5.0.0"),
            "copy-to-bind",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(path.to_string()),
                columndef_list_to_value(columns),
                opts_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("copy-to-bind dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "copy-to-bind", |p| lift_u32(p))
    }

    /// COPY TO: sink a batch of rows to the writer.
    pub fn copy_to_sink(
        &mut self,
        handle: u32,
        writer: u32,
        rows: &[Vec<Duckvalue>],
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let rows_val = wasmos_runtime_api::Value::List(
            rows.iter().map(|row| duckvalue_list_to_value(row)).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/copy-dispatch@5.0.0"),
            "copy-to-sink",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(writer),
                rows_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("copy-to-sink dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "copy-to-sink", |_| Ok(()))
    }

    /// COPY TO: finalize + close; returns rows written.
    pub fn copy_to_finalize(
        &mut self,
        handle: u32,
        writer: u32,
    ) -> Result<u64, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/copy-dispatch@5.0.0"),
            "copy-to-finalize",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(writer),
            ],
        ).map_err(|e| Duckerror::Internal(format!("copy-to-finalize dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "copy-to-finalize", |p| lift_u64(p))
    }

    /// COPY FROM: bind a reader for `path`, forwarding the destination table's
    /// `target_columns` (schema DuckDB has already resolved for e.g.
    /// `INSERT INTO t(a,b) FROM COPY ...`) into the guest bind. Returns
    /// (reader handle, columns).
    ///
    /// T1-6 landing: the copy-dispatch WIT `copy-from-bind` now carries
    /// `target-columns: list<columndef>`; the guest MUST prepare rows matching
    /// that schema. The host still validates returned-column arity against the
    /// target and rejects mismatches at bind time (see `ducklink_copy_from_bind`
    /// in reg_duckdb.rs). The prior `copy_from_bind_with_target` helper the
    /// sweep-2 prep introduced is retired — this is now the single entry point.
    pub fn copy_from_bind(
        &mut self,
        handle: u32,
        path: &str,
        options: &[(String, String)],
        target_columns: &[Columndef],
    ) -> Result<CopyFromBindResult, Duckerror> {
        // Phase 6.2.i.7 — migrated. copy-from-bind-result: record
        // { reader: u32, columns: list<columndef> }.
        use crate::export_marshal::*;
        let opts_val = wasmos_runtime_api::Value::List(
            options.iter().map(|(k, v)| wasmos_runtime_api::Value::Tuple(vec![
                wasmos_runtime_api::Value::String(k.clone()),
                wasmos_runtime_api::Value::String(v.clone()),
            ])).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/copy-dispatch@5.0.0"),
            "copy-from-bind",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(path.to_string()),
                opts_val,
                columndef_list_to_value(target_columns),
            ],
        ).map_err(|e| Duckerror::Internal(format!("copy-from-bind dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "copy-from-bind", |p| {
            let rec = p.ok_or_else(|| Duckerror::Internal(
                "copy-from-bind: expected Ok(copy-from-bind-result), got None".into()))?;
            let reader = u32_field(rec, "reader")?;
            let columns = value_to_columndef_list(record_field(rec, "columns")?)?;
            Ok(CopyFromBindResult { reader, columns })
        })
    }

    /// COPY FROM: pull up to `max_rows`; empty resultset signals EOF.
    pub fn copy_from_scan(
        &mut self,
        handle: u32,
        reader: u32,
        max_rows: u32,
    ) -> Result<Vec<Vec<Duckvalue>>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/copy-dispatch@5.0.0"),
            "copy-from-scan",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(reader),
                wasmos_runtime_api::Value::U32(max_rows),
            ],
        ).map_err(|e| Duckerror::Internal(format!("copy-from-scan dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "copy-from-scan", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "copy-from-scan: expected Ok(resultset), got None".into()))?;
            value_to_resultset(v)
        })
    }

    /// COPY FROM: close the reader.
    pub fn copy_from_close(
        &mut self,
        handle: u32,
        reader: u32,
    ) -> Result<bool, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/copy-dispatch@5.0.0"),
            "copy-from-close",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(reader),
            ],
        ).map_err(|e| Duckerror::Internal(format!("copy-from-close dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "copy-from-close", |p| lift_bool(p))
    }

    // --- 2.1.0 (Item 2): secret-dispatch re-entry ---

    /// Materialize a secret of `(type_name, provider)` from `params`; returns the
    /// resolved flat key=value set the core stores.
    pub fn create_secret(
        &mut self,
        handle: u32,
        type_name: &str,
        provider: &str,
        params: &[SecretKv],
    ) -> Result<Vec<SecretKv>, Duckerror> {
        // Phase 6.2.i.7 — migrated. secret-kv is record { key:
        // string, value: string }.
        use crate::export_marshal::*;
        let kv_to_val = |kv: &SecretKv| wasmos_runtime_api::Value::Record(vec![
            ("key".into(), wasmos_runtime_api::Value::String(kv.key.clone())),
            ("value".into(), wasmos_runtime_api::Value::String(kv.value.clone())),
        ]);
        let params_val = wasmos_runtime_api::Value::List(params.iter().map(kv_to_val).collect());
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/secret-dispatch@5.0.0"),
            "create-secret",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(type_name.to_string()),
                wasmos_runtime_api::Value::String(provider.to_string()),
                params_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("create-secret dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "create-secret", |p| {
            let list = p.ok_or_else(|| Duckerror::Internal(
                "create-secret: expected Ok(list<secret-kv>), got None".into()))?;
            let items = match list {
                wasmos_runtime_api::Value::List(items) => items,
                other => return Err(Duckerror::Internal(format!(
                    "create-secret: expected List, got {other:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(SecretKv {
                    key: string_field(item, "key")?,
                    value: string_field(item, "value")?,
                });
            }
            Ok(out)
        })
    }

    // --- 2.1.0 (Item 4): storage-write-dispatch re-entry ---

    /// Begin a write transaction on `catalog`; returns a transaction handle.
    pub fn storage_begin_transaction(
        &mut self,
        handle: u32,
        catalog: u32,
    ) -> Result<u32, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "begin-transaction",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(catalog),
            ],
        ).map_err(|e| Duckerror::Internal(format!("begin-transaction dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "begin-transaction", |p| lift_u32(p))
    }

    pub fn storage_commit_transaction(
        &mut self,
        handle: u32,
        txn: u32,
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "commit-transaction",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(txn),
            ],
        ).map_err(|e| Duckerror::Internal(format!("commit-transaction dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "commit-transaction", |_| Ok(()))
    }

    pub fn storage_rollback_transaction(
        &mut self,
        handle: u32,
        txn: u32,
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "rollback-transaction",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(txn),
            ],
        ).map_err(|e| Duckerror::Internal(format!("rollback-transaction dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "rollback-transaction", |_| Ok(()))
    }

    pub fn storage_create_table(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        columns: &[Columndef],
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated. columns: list<columndef> where
        // columndef = record { name: string, logical: logicaltype }.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "create-table",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(txn),
                wasmos_runtime_api::Value::String(table.to_string()),
                columndef_list_to_value(columns),
            ],
        ).map_err(|e| Duckerror::Internal(format!("create-table dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "create-table", |_| Ok(()))
    }

    pub fn storage_insert_rows(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        rows: &[Vec<Duckvalue>],
    ) -> Result<u64, Duckerror> {
        // Phase 6.2.i.7 — migrated. rows: list<list<duckvalue>>.
        use crate::export_marshal::*;
        let rows_val = wasmos_runtime_api::Value::List(
            rows.iter().map(|row| duckvalue_list_to_value(row)).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "insert-rows",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(txn),
                wasmos_runtime_api::Value::String(table.to_string()),
                rows_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("insert-rows dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "insert-rows", |p| lift_u64(p))
    }

    pub fn storage_delete_rows(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        rowids: &[i64],
    ) -> Result<u64, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "delete-rows",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(txn),
                wasmos_runtime_api::Value::String(table.to_string()),
                s64_list_to_value(rowids),
            ],
        ).map_err(|e| Duckerror::Internal(format!("delete-rows dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "delete-rows", |p| lift_u64(p))
    }

    pub fn storage_update_rows(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        rowids: &[i64],
        rows: &[Vec<Duckvalue>],
    ) -> Result<u64, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let rows_val = wasmos_runtime_api::Value::List(
            rows.iter().map(|row| duckvalue_list_to_value(row)).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "update-rows",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(txn),
                wasmos_runtime_api::Value::String(table.to_string()),
                s64_list_to_value(rowids),
                rows_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("update-rows dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "update-rows", |p| lift_u64(p))
    }

    /// Bug 4 fix: probe whether this backend expects the host to write
    /// through to the DSN file DIRECTLY (via a native file-backed
    /// connection) rather than round-tripping through
    /// `storage_serialize` + a full-file rewrite after each mutation. An
    /// extension whose backend runs in the wasm sandbox with no filesystem
    /// (e.g. sqlitewasm) returns `Ok(true)`; the host opens its own
    /// native file-backed connection at ATTACH time and replays each
    /// write there. Backends that persist natively (or that don't export
    /// storage-write-dispatch at all) return `Ok(false)` / an error;
    /// callers should treat those the same and keep the legacy
    /// serialize + write-back path.
    pub fn storage_writes_persist_directly(
        &mut self,
        handle: u32,
    ) -> Result<bool, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-write-dispatch@5.0.0"),
            "writes-persist-directly",
            &[wasmos_runtime_api::Value::U32(handle)],
        ).map_err(|e| Duckerror::Internal(format!("writes-persist-directly dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "writes-persist-directly", |p| lift_bool(p))
    }

    // --- 2.2.0 (Item 6): table-stream-dispatch re-entry ---
    // Drives a registered streaming/pushdown table function's exported
    // `table-stream-dispatch`. Types are remapped to base `extension_types` /
    // `extension_runtime` (see lib.rs `with`), so no per-world conversion.

    /// Open a streaming table cursor with bound `args` and a column `projection`
    /// (empty = all columns); returns the cursor handle + projected schema.
    pub fn table_open(
        &mut self,
        handle: u32,
        args: &[Duckvalue],
        projection: &[u32],
    ) -> Result<TableOpenResult, Duckerror> {
        // Phase 6.2.i.7 — migrated. table-open-result: record
        // { cursor: u32, columns: list<columndef> }.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/table-stream-dispatch@5.0.0"),
            "call-table-open",
            &[
                wasmos_runtime_api::Value::U32(handle),
                duckvalue_list_to_value(args),
                wasmos_runtime_api::Value::List(projection.iter().map(|n| wasmos_runtime_api::Value::U32(*n)).collect()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-table-open dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-table-open", |p| {
            let rec = p.ok_or_else(|| Duckerror::Internal(
                "call-table-open: expected Ok(table-open-result), got None".into()))?;
            let cursor = u32_field(rec, "cursor")?;
            let columns = value_to_columndef_list(record_field(rec, "columns")?)?;
            Ok(TableOpenResult { cursor, columns })
        })
    }

    /// 3.1.0: open a streaming table cursor WITH pushed-down filters (and a column
    /// `projection`, empty = all columns). `filters` is the conjunctive
    /// (AND-of-clauses) neutral filter set the core's streaming `TableFunction`
    /// extracted from the bound plan. A component that ignores the filters stays
    /// correct (the core re-checks them above the scan); honoring them prunes at
    /// the source. Drives the component's `call-table-open-filtered` export.
    pub fn table_open_filtered(
        &mut self,
        handle: u32,
        args: &[Duckvalue],
        projection: &[u32],
        filters: &[TableFilter],
    ) -> Result<TableOpenResult, Duckerror> {
        // Phase 6.2.i.7 — migrated. table-filter: record
        // { column: u32, op: filter-op, values: list<duckvalue> }.
        // filter-op enum: eq, ne, lt, le, gt, ge, is-in, is-null,
        // is-not-null.
        use crate::export_marshal::*;
        // Phase 6.2.k — FilterOp is now the ducklink-runtime-owned
        // mirror; enum-name mapping stays wire-canonical.
        let filter_op_val = |op: FilterOp| wasmos_runtime_api::Value::Enum(match op {
            FilterOp::Eq => "eq", FilterOp::Ne => "ne", FilterOp::Lt => "lt", FilterOp::Le => "le",
            FilterOp::Gt => "gt", FilterOp::Ge => "ge", FilterOp::IsIn => "is-in",
            FilterOp::IsNull => "is-null", FilterOp::IsNotNull => "is-not-null",
        }.into());
        let filters_val = wasmos_runtime_api::Value::List(
            filters.iter().map(|f| wasmos_runtime_api::Value::Record(vec![
                ("column".into(), wasmos_runtime_api::Value::U32(f.column)),
                ("op".into(), filter_op_val(f.op)),
                ("values".into(), duckvalue_list_to_value(&f.values)),
            ])).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/table-stream-dispatch@5.0.0"),
            "call-table-open-filtered",
            &[
                wasmos_runtime_api::Value::U32(handle),
                duckvalue_list_to_value(args),
                wasmos_runtime_api::Value::List(projection.iter().map(|n| wasmos_runtime_api::Value::U32(*n)).collect()),
                filters_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-table-open-filtered dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-table-open-filtered", |p| {
            let rec = p.ok_or_else(|| Duckerror::Internal(
                "call-table-open-filtered: expected Ok(table-open-result), got None".into()))?;
            let cursor = u32_field(rec, "cursor")?;
            let columns = value_to_columndef_list(record_field(rec, "columns")?)?;
            Ok(TableOpenResult { cursor, columns })
        })
    }

    /// Pull up to `max_rows` from the cursor; an empty resultset signals EOF.
    pub fn table_next(
        &mut self,
        handle: u32,
        cursor: u32,
        max_rows: u32,
    ) -> Result<Resultset, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/table-stream-dispatch@5.0.0"),
            "call-table-next",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(cursor),
                wasmos_runtime_api::Value::U32(max_rows),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-table-next dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-table-next", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-table-next: expected Ok(resultset), got None".into()))?;
            value_to_resultset(v)
        })
    }

    /// Close the streaming cursor and free its state.
    pub fn table_close(
        &mut self,
        handle: u32,
        cursor: u32,
    ) -> Result<bool, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/table-stream-dispatch@5.0.0"),
            "call-table-close",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(cursor),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-table-close dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-table-close", |p| lift_bool(p))
    }

    // --- 2.2.0 (Item 6): aggregate-incr-dispatch re-entry ---
    // Drives a registered incremental aggregate's init/update/combine/finalize
    // state machine.

    /// Allocate a fresh incremental-aggregate state; returns a state handle.
    pub fn aggregate_init(&mut self, handle: u32) -> Result<u32, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/aggregate-incr-dispatch@5.0.0"),
            "call-aggregate-init",
            &[wasmos_runtime_api::Value::U32(handle)],
        ).map_err(|e| Duckerror::Internal(format!("call-aggregate-init dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-aggregate-init", |p| lift_u32(p))
    }

    /// Fold a batch of `rows` into the aggregation `state`.
    pub fn aggregate_update(
        &mut self,
        handle: u32,
        state: u32,
        rows: &Rowbatch,
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated. Rowbatch = list<list<duckvalue>>.
        use crate::export_marshal::*;
        let rows_val = wasmos_runtime_api::Value::List(
            rows.iter().map(|row| duckvalue_list_to_value(row)).collect(),
        );
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/aggregate-incr-dispatch@5.0.0"),
            "call-aggregate-update",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(state),
                rows_val,
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-aggregate-update dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-aggregate-update", |_| Ok(()))
    }

    /// Merge the partial `source` state into `target` (parallel aggregation).
    pub fn aggregate_combine(
        &mut self,
        handle: u32,
        target: u32,
        source: u32,
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/aggregate-incr-dispatch@5.0.0"),
            "call-aggregate-combine",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(target),
                wasmos_runtime_api::Value::U32(source),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-aggregate-combine dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-aggregate-combine", |_| Ok(()))
    }

    /// Produce the final value from `state` and free it.
    pub fn aggregate_finalize(
        &mut self,
        handle: u32,
        state: u32,
    ) -> Result<Duckvalue, Duckerror> {
        // Phase 6.2.i.7 — migrated. Returns result<duckvalue, duckerror>.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/aggregate-incr-dispatch@5.0.0"),
            "call-aggregate-finalize",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(state),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-aggregate-finalize dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-aggregate-finalize", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-aggregate-finalize: expected Ok(duckvalue), got None".into()))?;
            value_to_duckvalue(v)
        })
    }

    // --- 2.2.0 (Item 7): conn-dispatch re-entry ---
    // Notifies a component that subscribed via lifecycle.register-connection-callback
    // when a connection is opened or closed.

    /// Notify the component that connection `connection_id` was opened.
    pub fn connection_opened(
        &mut self,
        handle: u32,
        connection_id: u64,
    ) -> Result<(), Duckerror> {
        // ADR-0029 Phase 6.2.i.5 — migrated from bindings.duckdb_
        // extension_conn_dispatch().call_on_connection_opened(store,
        // handle, connection_id) to sync_export_bridge::call_export.
        // Same wire semantics; return is result<_, duckerror> —
        // Ok(None) on success, Err(duckerror) on guest error.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/conn-dispatch@5.0.0"),
            "on-connection-opened",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U64(connection_id),
            ],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "on-connection-opened dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "on-connection-opened", |_| Ok(()))
    }

    /// Notify the component that connection `connection_id` was closed.
    pub fn connection_closed(
        &mut self,
        handle: u32,
        connection_id: u64,
    ) -> Result<(), Duckerror> {
        // ADR-0029 Phase 6.2.i.5 — sibling of connection_opened.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/conn-dispatch@5.0.0"),
            "on-connection-closed",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U64(connection_id),
            ],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "on-connection-closed dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "on-connection-closed", |_| Ok(()))
    }

    // --- 2.2.0 (Item 7): file-write-dispatch re-entry ---
    // Drives the writable + glob + stat half of a files backend.

    /// Write `data` at `offset` in `path`; returns the bytes written.
    pub fn file_write(
        &mut self,
        handle: u32,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/file-write-dispatch@5.0.0"),
            "file-write",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(path.to_string()),
                wasmos_runtime_api::Value::U64(offset),
                bytes_to_value(data),
            ],
        ).map_err(|e| Duckerror::Internal(format!("file-write dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "file-write", |p| lift_u64(p))
    }

    /// Expand a glob `pattern` to matching paths.
    pub fn file_glob(
        &mut self,
        handle: u32,
        pattern: &str,
    ) -> Result<Vec<String>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/file-write-dispatch@5.0.0"),
            "file-glob",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(pattern.to_string()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("file-glob dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "file-glob", |p| lift_string_list(p))
    }

    /// Stat a single `path`.
    pub fn file_stat(
        &mut self,
        handle: u32,
        path: &str,
    ) -> Result<FileInfo, Duckerror> {
        // Phase 6.2.i.7 — migrated. FileInfo record: (path, size,
        // is-directory) — decoded inline.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/file-write-dispatch@5.0.0"),
            "file-stat",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(path.to_string()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("file-stat dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "file-stat", |p| {
            let rec = p.ok_or_else(|| Duckerror::Internal(
                "file-stat: expected Ok(file-info), got None".into()))?;
            Ok(FileInfo {
                path: string_field(rec, "path")?,
                size: u64_field(rec, "size")?,
                is_directory: bool_field(rec, "is-directory")?,
            })
        })
    }

    // --- 2.2.0 (Item 7): index-write-dispatch re-entry ---
    // Drives the general (non-ANN) secondary-index operations: ranged scan,
    // delete, unique-constraint check, and serialization.

    /// Range scan: row-ids whose key is within [low, high] (empty = unbounded).
    pub fn index_scan(
        &mut self,
        handle: u32,
        index: u32,
        low: &[Duckvalue],
        high: &[Duckvalue],
    ) -> Result<Vec<i64>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-write-dispatch@5.0.0"),
            "index-scan",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(index),
                duckvalue_list_to_value(low),
                duckvalue_list_to_value(high),
            ],
        ).map_err(|e| Duckerror::Internal(format!("index-scan dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "index-scan", |p| lift_s64_list(p))
    }

    /// Delete the given `rowids` from the index; returns the number removed.
    pub fn index_delete(
        &mut self,
        handle: u32,
        index: u32,
        rowids: &[i64],
    ) -> Result<u64, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-write-dispatch@5.0.0"),
            "index-delete",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(index),
                s64_list_to_value(rowids),
            ],
        ).map_err(|e| Duckerror::Internal(format!("index-delete dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "index-delete", |p| lift_u64(p))
    }

    /// Unique-constraint check: true iff inserting `keys` would violate uniqueness.
    pub fn index_constraint(
        &mut self,
        handle: u32,
        index: u32,
        keys: &[Duckvalue],
    ) -> Result<bool, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-write-dispatch@5.0.0"),
            "index-constraint",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(index),
                duckvalue_list_to_value(keys),
            ],
        ).map_err(|e| Duckerror::Internal(format!("index-constraint dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "index-constraint", |p| lift_bool(p))
    }

    /// Serialize the built index to bytes for persistence.
    pub fn index_serialize(
        &mut self,
        handle: u32,
        index: u32,
    ) -> Result<Vec<u8>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-write-dispatch@5.0.0"),
            "index-serialize",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(index),
            ],
        ).map_err(|e| Duckerror::Internal(format!("index-serialize dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "index-serialize", |p| lift_bytes(p))
    }

    // --- 2.2.0 (Item 7): settings-dispatch re-entry ---
    // Notifies a component that declared an option (via settings.register-option)
    // and exported settings-dispatch when a user runs `SET <name> = <value>`.

    /// Notify the component that option `name` was SET to `value` (rendered text).
    pub fn setting_set(
        &mut self,
        handle: u32,
        name: &str,
        value: &str,
    ) -> Result<(), Duckerror> {
        // ADR-0029 Phase 6.2.i.5 — migrated from bindings.duckdb_
        // extension_settings_dispatch().call_on_setting_set(store,
        // handle, name, value) to sync_export_bridge::call_export.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/settings-dispatch@5.0.0"),
            "on-setting-set",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(name.to_string()),
                wasmos_runtime_api::Value::String(value.to_string()),
            ],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "on-setting-set dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "on-setting-set", |_| Ok(()))
    }

    // --- 3.2.0: log-storage-dispatch re-entry ---
    // Forwards one DuckDB log record to the component whose `callback-handle`
    // matches a storage the guest registered via `log-storage.register-log-storage`
    // (Class B parity with the stable `duckdb_register_log_storage` C API). The
    // bindings are built lazily from the SAME loaded component instance so a
    // non-log-sink extension pays nothing.

    /// Deliver one log entry to the component's registered log sink. `handle` is
    /// the `callback-handle` the component passed to `register-log-storage`; the
    /// C API installer in `ducklink-extension/src/reg_duckdb.rs` wires each
    /// `duckdb_register_log_storage` write callback to this method.
    pub fn dispatch_write_log_entry(
        &mut self,
        handle: u32,
        entry: LogEntry,
    ) -> Result<(), Duckerror> {
        // ADR-0029 Phase 6.2.i.5 — migrated. LogEntry is a WIT
        // record with fields (level: u32, message: string,
        // tags: option<list<tuple<string, string>>>, ts-micros:
        // s64). Wire field names use WIT canonical hyphenation
        // (e.g. `ts-micros`, not `ts_micros`); wit-bindgen renames
        // to snake_case for Rust identifiers but the record wire
        // shape wasmtime's Val::Record consumes uses the WIT
        // spelling.
        let tags_val = match entry.tags {
            Some(pairs) => wasmos_runtime_api::Value::Option(Some(Box::new(
                wasmos_runtime_api::Value::List(
                    pairs
                        .into_iter()
                        .map(|(k, v)| {
                            wasmos_runtime_api::Value::Tuple(vec![
                                wasmos_runtime_api::Value::String(k),
                                wasmos_runtime_api::Value::String(v),
                            ])
                        })
                        .collect(),
                ),
            ))),
            None => wasmos_runtime_api::Value::Option(None),
        };
        let entry_val = wasmos_runtime_api::Value::Record(vec![
            ("level".to_string(), wasmos_runtime_api::Value::U32(entry.level)),
            (
                "message".to_string(),
                wasmos_runtime_api::Value::String(entry.message.clone()),
            ),
            ("tags".to_string(), tags_val),
            ("ts-micros".to_string(), wasmos_runtime_api::Value::S64(entry.ts_micros)),
        ]);
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/log-storage-dispatch@5.0.0"),
            "write-log-entry",
            &[wasmos_runtime_api::Value::U32(handle), entry_val],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "write-log-entry dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "write-log-entry", |_| Ok(()))
    }

    // --- 4.0.0: arrow-ext-dispatch re-entry ---
    // Drives the guest's 3-fn cursor (`call-arrow-open` / `-next` / `-close`)
    // for a component that registered a named Arrow producer via
    // `arrow-ext.register-arrow-table`. Cursor state lives entirely on the
    // guest; the host holds only the opaque cursor u32 and pulls row-vector
    // batches (`resultset`) until an empty resultset signals EOF. Bindings are
    // built lazily from the SAME loaded component instance so a non-arrow-
    // producer extension pays nothing.

    /// Open a scan cursor against the arrow producer named by `callback_handle`.
    /// Returns the guest-side opaque cursor id (which the host then threads
    /// through subsequent `dispatch_arrow_next` / `dispatch_arrow_close` calls).
    pub fn dispatch_arrow_open(
        &mut self,
        callback_handle: u32,
    ) -> Result<u32, Duckerror> {
        // ADR-0029 Phase 6.2.i.6 — migrated. Returns result<u32,
        // duckerror> — Ok payload is the guest-assigned cursor id.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/arrow-ext-dispatch@5.0.0"),
            "call-arrow-open",
            &[wasmos_runtime_api::Value::U32(callback_handle)],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "call-arrow-open dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "call-arrow-open", |payload| match payload {
            Some(wasmos_runtime_api::Value::U32(n)) => Ok(*n),
            Some(other) => Err(Duckerror::Internal(format!(
                "call-arrow-open: expected Ok(U32), got {other:?}"
            ))),
            None => Err(Duckerror::Internal(
                "call-arrow-open: expected Ok(U32), got Ok(None)".to_string(),
            )),
        })
    }

    /// Pull the next batch of rows from the guest cursor. An empty resultset
    /// signals EOF; the caller then invokes `dispatch_arrow_close` to release
    /// the cursor state.
    pub fn dispatch_arrow_next(
        &mut self,
        callback_handle: u32,
        cursor: u32,
    ) -> Result<Resultset, Duckerror> {
        // Phase 6.2.i.7 — migrated. Resultset = list<list<duckvalue>>.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/arrow-ext-dispatch@5.0.0"),
            "call-arrow-next",
            &[
                wasmos_runtime_api::Value::U32(callback_handle),
                wasmos_runtime_api::Value::U32(cursor),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-arrow-next dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-arrow-next", |p| {
            let rec = p.ok_or_else(|| Duckerror::Internal(
                "call-arrow-next: expected Ok(resultset), got None".into()))?;
            value_to_resultset(rec)
        })
    }

    /// Close the guest cursor and release its state. Returns whether the
    /// cursor was known to the guest.
    pub fn dispatch_arrow_close(
        &mut self,
        callback_handle: u32,
        cursor: u32,
    ) -> Result<bool, Duckerror> {
        // ADR-0029 Phase 6.2.i.6 — migrated. Returns result<bool,
        // duckerror>.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/arrow-ext-dispatch@5.0.0"),
            "call-arrow-close",
            &[
                wasmos_runtime_api::Value::U32(callback_handle),
                wasmos_runtime_api::Value::U32(cursor),
            ],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "call-arrow-close dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "call-arrow-close", |payload| match payload {
            Some(wasmos_runtime_api::Value::Bool(b)) => Ok(*b),
            Some(other) => Err(Duckerror::Internal(format!(
                "call-arrow-close: expected Ok(Bool), got {other:?}"
            ))),
            None => Err(Duckerror::Internal(
                "call-arrow-close: expected Ok(Bool), got Ok(None)".to_string(),
            )),
        })
    }

    // --- M2a: storage-dispatch (foreign-catalog) re-entry ---
    // Mirrors the callback-dispatch `dispatch_*` methods but drives the
    // component's exported `storage-dispatch` interface. The native host stages
    // the foreign DB bytes (attach-blob) then attaches, so `storage_attach`
    // reads the host file at `dsn` and hands the bytes to the component.

    /// Stage `bytes` under `dsn`, then open the catalog. Returns the
    /// component-side catalog handle. `handle` is the storage backend's
    /// callback-handle (passed by the component to register-storage).
    pub fn storage_attach(
        &mut self,
        handle: u32,
        dsn: &str,
        bytes: &[u8],
    ) -> Result<u32, Duckerror> {
        // Phase 6.2.i.7 — migrated. Two-step: attach-blob then
        // storage-attach with empty bytes (bytes were staged via
        // attach-blob).
        use crate::export_marshal::*;
        // Step 1: attach-blob(handle, dsn, bytes) -> result<_, duckerror>
        let out1 = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "attach-blob",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(dsn.to_string()),
                bytes_to_value(bytes),
            ],
        ).map_err(|e| Duckerror::Internal(format!("attach-blob dispatch failed: {e}")))?;
        export_result_to_duckerror(out1, "attach-blob", |_| Ok(()))?;
        // Step 2: storage-attach(handle, dsn, empty) -> result<u32, duckerror>
        let out2 = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "storage-attach",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(dsn.to_string()),
                wasmos_runtime_api::Value::List(Vec::new()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage-attach dispatch failed: {e}")))?;
        export_result_to_duckerror(out2, "storage-attach", |p| lift_u32(p))
    }

    pub fn storage_list_tables(
        &mut self,
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<String>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "storage-list-tables",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(catalog),
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage-list-tables dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "storage-list-tables", |p| lift_string_list(p))
    }

    pub fn storage_table_columns(
        &mut self,
        handle: u32,
        catalog: u32,
        table: &str,
    ) -> Result<Vec<Columndef>, Duckerror> {
        // Phase 6.2.i.7 — migrated. Returns result<list<columndef>,
        // duckerror>; columndef decoded via value_to_columndef_list.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "storage-table-columns",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(catalog),
                wasmos_runtime_api::Value::String(table.to_string()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage-table-columns dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "storage-table-columns", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "storage-table-columns: expected Ok(list<columndef>), got None".into()))?;
            value_to_columndef_list(v)
        })
    }

    /// M2b: open a scan cursor for `(catalog, table)` honoring the request's
    /// projection + filters + limit. Returns the component-side scan handle.
    pub fn storage_scan_open(
        &mut self,
        handle: u32,
        catalog: u32,
        request: storage_scan::ScanRequest,
    ) -> Result<u32, Duckerror> {
        // Phase 6.2.i.7 — migrated. Phase 6.2.l' — `storage_scan`
        // types now mirror the WIT records directly (no per-world
        // Duckvalue clone), so filter values marshal through the
        // shared `duckvalue_to_value` without a bridge.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "storage-scan-open",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(catalog),
                {
                    use storage_scan::CompareOp as C;
                    let op_val = |op: C| wasmos_runtime_api::Value::Enum(match op {
                        C::Eq => "eq", C::Ne => "ne", C::Lt => "lt", C::Le => "le",
                        C::Gt => "gt", C::Ge => "ge", C::IsNull => "is-null",
                        C::IsNotNull => "is-not-null",
                    }.into());
                    let filters_val = wasmos_runtime_api::Value::List(
                        request.filters.iter().map(|f| wasmos_runtime_api::Value::Record(vec![
                            ("column".into(), wasmos_runtime_api::Value::U32(f.column)),
                            ("op".into(), op_val(f.op)),
                            ("value".into(), duckvalue_to_value(&f.value)),
                        ])).collect(),
                    );
                    wasmos_runtime_api::Value::Record(vec![
                        ("table".into(), wasmos_runtime_api::Value::String(request.table.clone())),
                        ("projection".into(), wasmos_runtime_api::Value::List(
                            request.projection.iter().map(|n| wasmos_runtime_api::Value::U32(*n)).collect(),
                        )),
                        ("filters".into(), filters_val),
                        ("limit".into(), wasmos_runtime_api::Value::Option(
                            request.limit.map(|n| Box::new(wasmos_runtime_api::Value::U64(n))),
                        )),
                    ])
                },
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage-scan-open dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "storage-scan-open", |p| lift_u32(p))
    }

    /// M2b: pull up to `max_rows` rows from a scan; empty resultset signals EOF.
    pub fn storage_scan_next(
        &mut self,
        handle: u32,
        scan: u32,
        max_rows: u32,
    ) -> Result<Vec<Vec<Duckvalue>>, Duckerror> {
        // Phase 6.2.i.7 — migrated. Returns resultset =
        // list<list<duckvalue>>; wire duckvalue is canonical WIT +
        // value_to_resultset decodes directly into
        // Vec<Vec<Duckvalue>>.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "storage-scan-next",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(scan),
                wasmos_runtime_api::Value::U32(max_rows),
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage-scan-next dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "storage-scan-next", |p| {
            let rec = p.ok_or_else(|| Duckerror::Internal(
                "storage-scan-next: expected Ok(resultset), got None".into()))?;
            value_to_resultset(rec)
        })
    }

    /// M2b: close a scan cursor.
    pub fn storage_scan_close(
        &mut self,
        handle: u32,
        scan: u32,
    ) -> Result<bool, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "storage-scan-close",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(scan),
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage-scan-close dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "storage-scan-close", |p| lift_bool(p))
    }

    /// AT5 write-back: serialize the extension's in-memory representation of
    /// `catalog` back to a byte blob. The host writes those bytes back to the
    /// ATTACH DSN file path after each successful INSERT/UPDATE/DELETE dispatch
    /// so the mutation persists on disk (sqlitewasm's storage lives in an
    /// in-memory `sqlite3_deserialize`d copy; without this the writes never
    /// leave the wasm heap). Extensions whose backend isn't a serializable blob
    /// (e.g. remote MySQL/Postgres connections) return
    /// `Duckerror::Unsupported`, which the host treats as a silent no-op.
    pub fn storage_serialize(
        &mut self,
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<u8>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/storage-dispatch@5.0.0"),
            "serialize",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(catalog),
            ],
        ).map_err(|e| Duckerror::Internal(format!("storage.serialize dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "storage.serialize", |p| lift_bytes(p))
    }

    // --- Item 3 / M2a: index-dispatch (custom index build + search) re-entry ---
    // Mirrors the storage-dispatch `storage_*` methods but drives the component's
    // exported `index-dispatch` interface. The HNSW (or other ANN) build happens
    // in-component over a create -> append -> build lifecycle; search returns kNN
    // hits. No callback-handle is threaded (the component keys index state by
    // index NAME), so these take no `handle` argument.

    /// Allocate an empty index builder for `(type_name, index_name)` over a
    /// FLOAT[dims] key. Returns the component-side index-handle.
    pub fn index_create(
        &mut self,
        type_name: &str,
        index_name: &str,
        dims: u32,
    ) -> Result<u32, Duckerror> {
        // ADR-0029 Phase 6.2.i.6 — migrated. `index_duckerror_to_ext`
        // (a bindings-per-module Duckerror converter) is no longer
        // needed because the wire duckerror shape is canonical WIT +
        // `duckerror_from_value` decodes directly into
        // `Duckerror`.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-dispatch@5.0.0"),
            "index-create",
            &[
                wasmos_runtime_api::Value::String(type_name.to_string()),
                wasmos_runtime_api::Value::String(index_name.to_string()),
                wasmos_runtime_api::Value::U32(dims),
            ],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "index-create dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "index-create", |payload| match payload {
            Some(wasmos_runtime_api::Value::U32(n)) => Ok(*n),
            Some(other) => Err(Duckerror::Internal(format!(
                "index-create: expected Ok(U32), got {other:?}"
            ))),
            None => Err(Duckerror::Internal(
                "index-create: expected Ok(U32), got Ok(None)".to_string(),
            )),
        })
    }

    /// Accumulate a batch of (rowid, vector) rows into the builder.
    pub fn index_append(
        &mut self,
        handle: u32,
        rowids: &[i64],
        vectors: &[Vec<f32>],
    ) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-dispatch@5.0.0"),
            "index-append",
            &[
                wasmos_runtime_api::Value::U32(handle),
                s64_list_to_value(rowids),
                f32_matrix_to_value(vectors),
            ],
        ).map_err(|e| Duckerror::Internal(format!("index-append dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "index-append", |_| Ok(()))
    }

    /// Finalize: build the ANN map from every appended row.
    pub fn index_build(&mut self, handle: u32) -> Result<(), Duckerror> {
        // ADR-0029 Phase 6.2.i.6 — migrated.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-dispatch@5.0.0"),
            "index-build",
            &[wasmos_runtime_api::Value::U32(handle)],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "index-build dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "index-build", |_| Ok(()))
    }

    /// k nearest neighbours of `query`, closest first.
    pub fn index_search(
        &mut self,
        handle: u32,
        query: &[f32],
        k: u32,
    ) -> Result<Vec<IndexHit>, Duckerror> {
        // Phase 6.2.i.7 — migrated. Returns result<list<index-hit>,
        // duckerror> where index-hit is record { rowid: s64, distance:
        // f32 } — decoded inline.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-dispatch@5.0.0"),
            "index-search",
            &[
                wasmos_runtime_api::Value::U32(handle),
                f32_list_to_value(query),
                wasmos_runtime_api::Value::U32(k),
            ],
        ).map_err(|e| Duckerror::Internal(format!("index-search dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "index-search", |p| {
            let list = p.ok_or_else(|| Duckerror::Internal(
                "index-search: expected Ok(list<index-hit>), got None".into()))?;
            let items = match list {
                wasmos_runtime_api::Value::List(items) => items,
                other => return Err(Duckerror::Internal(format!(
                    "index-search: expected List, got {other:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(IndexHit {
                    rowid: s64_field(item, "rowid")?,
                    distance: match record_field(item, "distance")? {
                        wasmos_runtime_api::Value::F32(f) => *f,
                        o => return Err(Duckerror::Internal(format!(
                            "index-hit.distance: expected F32, got {o:?}"))),
                    },
                });
            }
            Ok(out)
        })
    }

    /// Free the index + handle.
    pub fn index_drop(&mut self, handle: u32) -> Result<(), Duckerror> {
        // ADR-0029 Phase 6.2.i.6 — migrated.
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/index-dispatch@5.0.0"),
            "index-drop",
            &[wasmos_runtime_api::Value::U32(handle)],
        )
        .map_err(|e| Duckerror::Internal(format!(
            "index-drop dispatch failed: {e}"
        )))?;
        export_result_to_duckerror(out, "index-drop", |_| Ok(()))
    }

    // --- httpfs M2: file-dispatch (remote file I/O) re-entry ---
    // Mirrors the storage-dispatch `storage_*` methods but drives the files
    // backend component's exported `file-dispatch` interface. The component
    // fetches the whole resource over wasi:sockets at open, caches it, and
    // serves byte ranges. The error channel is plain strings (not duckerror).

    /// Open (fetch + cache) `url`. Returns (component-side file handle, size).
    /// `handle` is the files backend's callback-handle (from register-files).
    pub fn file_open(
        &mut self,
        handle: u32,
        url: &str,
    ) -> Result<(u32, u64), Duckerror> {
        // Phase 6.2.i.7 — migrated. Return is result<file-open-result,
        // string> (NOT duckerror — file-dispatch uses string errs);
        // file-open-result record has (handle: u32, size: u64).
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/file-dispatch@5.0.0"),
            "file-open",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(url.to_string()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("file-open dispatch failed: {e}")))?;
        export_result_to_string(out, "file-open", |p| {
            let rec = p.ok_or_else(|| "expected Ok(file-open-result), got None".to_string())?;
            let h = u32_field(rec, "handle").map_err(|e| format!("{e:?}"))?;
            let s = u64_field(rec, "size").map_err(|e| format!("{e:?}"))?;
            Ok((h, s))
        })
    }

    /// Read up to `len` bytes from `file` at `offset`. A short read at EOF is
    /// allowed.
    pub fn file_read(
        &mut self,
        handle: u32,
        file: u32,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/file-dispatch@5.0.0"),
            "file-read",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(file),
                wasmos_runtime_api::Value::U64(offset),
                wasmos_runtime_api::Value::U32(len),
            ],
        ).map_err(|e| Duckerror::Internal(format!("file-read dispatch failed: {e}")))?;
        export_result_to_string(out, "file-read", |p| {
            lift_bytes(p).map_err(|e| format!("{e:?}"))
        })
    }

    /// Drop the component-side cache entry for `file`.
    pub fn file_close(&mut self, handle: u32, file: u32) -> Result<(), Duckerror> {
        // Phase 6.2.i.7 — migrated.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/file-dispatch@5.0.0"),
            "file-close",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::U32(file),
            ],
        ).map_err(|e| Duckerror::Internal(format!("file-close dispatch failed: {e}")))?;
        export_result_to_string(out, "file-close", |_| Ok(()))
    }

    // 2.3.0 / v3: lazily-built parser-dispatch bindings.

    /// Offer the unrecognized statement `query` to the parser extension `handle`.
    /// Returns `Some(rewrite_sql)` if the component claims it (string->SQL rewrite),
    /// or `None` if it declines. Drives `parser-dispatch.call-parse`.
    pub fn call_parse(
        &mut self,
        handle: u32,
        query: &str,
    ) -> Result<Option<String>, Duckerror> {
        // Phase 6.2.i.7 — migrated. Returns result<parse-outcome,
        // duckerror> where parse-outcome is variant { declined,
        // rewrite(string) } — decoded inline.
        use crate::export_marshal::*;
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/parser-dispatch@5.0.0"),
            "call-parse",
            &[
                wasmos_runtime_api::Value::U32(handle),
                wasmos_runtime_api::Value::String(query.to_string()),
            ],
        ).map_err(|e| Duckerror::Internal(format!("call-parse dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-parse", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-parse: expected Ok(parse-outcome), got None".into()))?;
            match v {
                wasmos_runtime_api::Value::Variant { discriminant, payload } => match discriminant.as_str() {
                    "declined" => Ok(None),
                    "rewrite" => match payload.as_deref() {
                        Some(wasmos_runtime_api::Value::String(s)) => Ok(Some(s.clone())),
                        other => Err(Duckerror::Internal(format!(
                            "parse-outcome.rewrite: expected String, got {other:?}"))),
                    },
                    other => Err(Duckerror::Internal(format!(
                        "parse-outcome: unknown discriminant {other:?}"))),
                },
                other => Err(Duckerror::Internal(format!(
                    "call-parse: expected Variant, got {other:?}"))),
            }
        })
    }

    // 2.3.0 / v3: lazily-built optimizer-dispatch bindings.

    /// Offer the flattened plan (`nodes` = (id, op-type, parent, params-json);
    /// `query` = the source SQL or empty) to the optimizer rule `handle`. Returns
    /// `Some(rewrite_sql)` for a `rewrite-query` directive, or `None` for declined /
    /// a structured `apply` directive (not driven via SQL re-plan). Drives
    /// `optimizer-dispatch.call-optimize`.
    pub fn call_optimize(
        &mut self,
        handle: u32,
        nodes: Vec<(u32, String, Option<u32>, String)>,
        query: &str,
    ) -> Result<Option<String>, Duckerror> {
        // Phase 6.2.i.7 — migrated. plan-shape is
        // record { nodes: list<plan-node>, query: string }
        // where plan-node is record { id: u32, op-type: string,
        // parent: option<u32>, params-json: string }.
        // rewrite-directive is variant { declined,
        // rewrite-query(string), apply(...structured...) }; the
        // structured Apply arm was already collapsed to None in the
        // old code, so we do the same here — decode the variant and
        // ignore Apply-arm's payload structure.
        use crate::export_marshal::*;
        let plan_nodes_val = wasmos_runtime_api::Value::List(
            nodes.into_iter().map(|(id, op_type, parent, params_json)| {
                wasmos_runtime_api::Value::Record(vec![
                    ("id".into(), wasmos_runtime_api::Value::U32(id)),
                    ("op-type".into(), wasmos_runtime_api::Value::String(op_type)),
                    ("parent".into(), wasmos_runtime_api::Value::Option(
                        parent.map(|p| Box::new(wasmos_runtime_api::Value::U32(p))),
                    )),
                    ("params-json".into(), wasmos_runtime_api::Value::String(params_json)),
                ])
            }).collect(),
        );
        let plan_val = wasmos_runtime_api::Value::Record(vec![
            ("nodes".into(), plan_nodes_val),
            ("query".into(), wasmos_runtime_api::Value::String(query.to_string())),
        ]);
        let out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
            self.store.as_context_mut(),
            &self.instance,
            Some("duckdb:extension/optimizer-dispatch@5.0.0"),
            "call-optimize",
            &[wasmos_runtime_api::Value::U32(handle), plan_val],
        ).map_err(|e| Duckerror::Internal(format!("call-optimize dispatch failed: {e}")))?;
        export_result_to_duckerror(out, "call-optimize", |p| {
            let v = p.ok_or_else(|| Duckerror::Internal(
                "call-optimize: expected Ok(rewrite-directive), got None".into()))?;
            match v {
                wasmos_runtime_api::Value::Variant { discriminant, payload } => match discriminant.as_str() {
                    "declined" => Ok(None),
                    "rewrite-query" => match payload.as_deref() {
                        Some(wasmos_runtime_api::Value::String(s)) => Ok(Some(s.clone())),
                        other => Err(Duckerror::Internal(format!(
                            "rewrite-directive.rewrite-query: expected String, got {other:?}"))),
                    },
                    "apply" => Ok(None),  // Structured rewrites collapse to declined per legacy behavior.
                    other => Err(Duckerror::Internal(format!(
                        "rewrite-directive: unknown discriminant {other:?}"))),
                },
                other => Err(Duckerror::Internal(format!(
                    "call-optimize: expected Variant, got {other:?}"))),
            }
        })
    }
}

// T1-7: fire `guest.shutdown` before the store tears down. This is the only
// hook the component has for flushing external resources it owns (a background
// writer, an open file handle, a network client) — after Drop returns, the
// wasmtime store is gone and no guest export is reachable. Ordering matters:
// the shutdown call must happen BEFORE the fields of `self` (in declaration
// order — `store` first) are dropped, which is exactly what a `Drop::drop`
// body running before automatic field-drop gives us.
//
// A trap or panic from the guest during shutdown must NOT abort the process:
// Drop can't return an error, so we log any failure to stderr and continue.
// `catch_unwind` wraps the whole dispatch so a panic mid-drop (e.g. a
// wasmtime invariant tripping during teardown) becomes a logged line, not a
// process abort. `AssertUnwindSafe` is acceptable here because Drop is the
// terminal action for this instance — no shared state survives to observe a
// half-transitioned mutation.
//
// TODO T3-1 (reconfigure): `dispatch_reconfigure` awaits a per-option SET
// notification hook in the DuckDB stable C API (no `duckdb_config_option_
// on_set` or equivalent shipped). Not wired here — a Drop-time fire would
// be the wrong shape (reconfigure is a mid-life event, not shutdown).
impl Drop for ExtensionInstance {
    fn drop(&mut self) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch_shutdown()));
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                eprintln!("[ducklink] component shutdown failed: {err:?}");
            }
            Err(_) => {
                eprintln!("[ducklink] component shutdown panicked; continuing teardown");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: the pure capture conversions + the capture-into-pending logic.
//
// These exercise the trust-boundary converters (a component-supplied WIT value
// turned into a neutral `reg::*`) and the storage/index world -> base-world
// converters WITHOUT needing wasmtime to instantiate a component. The Host
// trait impls that capture registrations DO need an `ExtensionStoreState`, which
// we build with a no-op services sink and an empty wasi context.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // ADR-0029 Phase 6.2.g — NoopServices + test_state moved to
    // crate::extension_test_support so extension_wasmos::tests can
    // share the same fixture. Re-exported into this module's scope
    // via `use` so the many downstream test callsites below need no
    // further edits.
    use crate::extension_test_support::{test_state, NoopServices};

    /// Every base-world logicaltype, including the rich set, for round-tripping.
    fn all_ext_logicaltypes() -> Vec<Logicaltype> {
        use Logicaltype as L;
        vec![
            L::Boolean,
            L::Int64,
            L::Uint64,
            L::Float64,
            L::Text,
            L::Blob,
            L::Int32,
            L::Timestamp,
            L::Int8,
            L::Int16,
            L::Uint8,
            L::Uint16,
            L::Uint32,
            L::Float32,
            L::Date,
            L::Time,
            L::Timestamptz,
            L::Decimal(Decimalshape { width: 18, scale: 3 }),
            L::Hugeint,
            L::Uhugeint,
            L::Interval,
            L::Uuid,
            L::Complex("STRUCT(a INTEGER, b VARCHAR)".to_string()),
        ]
    }

    /// Build a component-model engine (with wasm-exceptions) the way the host does.
    fn test_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.wasm_exceptions(true);
        Engine::new(&config).expect("engine")
    }

    fn load_artifact(engine: &Engine, name: &str) -> wasmtime::Result<ExtensionInstance> {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions")
            .join(format!("{name}.wasm"));
        let bytes = std::fs::read(&path).expect("read artifact");
        let component = Component::new(engine, &bytes)?;
        let wasi = wasmtime_wasi::WasiCtxBuilder::new()
            .inherit_stderr()
            .build();
        load_component(
            engine,
            &component,
            wasi,
            Box::new(NoopServices),
            Arc::new(RwLock::new(CallbackRegistry::default())),
            name.to_string(),
        )
    }

    /// The @5.0.0 contract check accepts the coordinated post-rebuild
    /// artifacts. Historical context: the @4.x → @5.0.0 major bump
    /// (S1 nested-type collapse, S2 first-class decimal, T2-1 hugeint
    /// drop) was a DELIBERATE clean break — pre-existing @4.x
    /// components were rejected by design until every artifact
    /// rebuilt. Post-rebuild (current state), `component_contract_
    /// version` reports @5.0.0 for the on-disk artifacts and
    /// `check_component_contract` accepts them.
    ///
    /// Phase 6.2.h.7 REPLACED this test's original intent (assert the
    /// @4.x rejection) with the post-rebuild assertion (assert the
    /// @5.x acceptance) after all artifacts were rebuilt. The break
    /// itself is proven at the release-boundary level via
    /// datalink-contract's own tests.
    #[test]
    fn major_5_accepts_post_rebuild_artifacts() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let aba = std::path::Path::new(manifest).join("../../artifacts/extensions/aba.wasm");
        if !aba.exists() {
            eprintln!("skipping post-rebuild acceptance test: artifacts/extensions/aba.wasm absent");
            return;
        }

        assert_eq!(crate::CONTRACT_MAJOR, 5);
        assert_eq!(crate::CONTRACT_MINOR, 0);

        let engine = test_engine();
        for name in ["aba", "geohash"] {
            let path = std::path::Path::new(manifest)
                .join("../../artifacts/extensions")
                .join(format!("{name}.wasm"));
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let component = Component::new(&engine, &bytes).unwrap();

            // Post-rebuild: artifact reports @5.
            let ver = crate::component_contract_version(&engine, &component);
            assert_eq!(
                ver.map(|(maj, _)| maj),
                Some(5),
                "{name} on disk is expected to be the @5.0.0 rebuild"
            );

            // The major-5 contract guard ACCEPTS it.
            assert!(
                crate::check_component_contract(&engine, &component, name).is_ok(),
                "{name}: a @5.x component MUST be accepted by the major-5 host"
            );
        }
    }

    /// ADR-0029 Phase 6.2.h.8 — end-to-end proof that ducklink's
    /// production `load_component` flow (which now dispatches every
    /// extension SPI host import through the wasmos-native install
    /// path via `install_wasmos_migrated_interfaces`) successfully
    /// loads a real @5.0.0 component.
    ///
    /// Loads `pintest_a.wasm` because it imports the full
    /// `duckdb:extension/runtime` interface (10 resource types incl.
    /// the get-capability multi-arm variant) — the exact surface Phase
    /// 6.2.h.7 unblocked via multi-resource classification on the
    /// resource-aware sync bridge. Successful load = every one of the
    /// 27 wasmos-native handlers wires cleanly through the bridge, the
    /// contract guard passes at @5.0.0, and the guest's `load()`
    /// export runs to completion under the wasmos-native dispatch
    /// path (invoking `types::` + `runtime::` host imports as needed).
    ///
    /// Skipped gracefully if the artifact is absent (toolchain-free
    /// CI subset). Requires the coordinated rebuild that has landed.
    #[test]
    fn wasmos_native_load_pintest_a_end_to_end() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions/pintest_a.wasm");
        if !path.exists() {
            eprintln!(
                "skipping wasmos-native e2e load test: {} absent",
                path.display()
            );
            return;
        }

        let engine = test_engine();
        let instance = load_artifact(&engine, "pintest_a")
            .expect("pintest_a should load through the wasmos-native install path");

        // The load succeeded: contract guard accepted @5.0.0, every
        // extension SPI interface pintest_a imports resolved through
        // `install_wasmos_migrated_interfaces` + the sync bridge, and
        // the component's `load()` export ran to completion (which
        // may have exercised `types::` + `runtime::` host-import
        // dispatches via the wasmos-native handlers).
        let _ = instance; // retained by ExtensionInstance's Drop —
                          // any pending registrations were captured
                          // into the store's ExtensionStoreState.
        eprintln!(
            "[wasmos-native] pintest_a loaded successfully through the wasmos-\
             migrated install path (27/27 interfaces)"
        );
    }

    /// Arc 2 (behavior test) — the pintest sibling artifact, an
    /// independent-shape verification that `install_wasmos_migrated_
    /// interfaces` handles a DIFFERENT component-model shape than
    /// pintest_a. If pintest_a passes and pintest_b fails, the failure
    /// is localized to component-shape sensitivity in the install
    /// path, not the wasmos handler set itself.
    #[test]
    fn wasmos_native_load_pintest_b_end_to_end() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions/pintest_b.wasm");
        if !path.exists() {
            eprintln!(
                "skipping wasmos-native pintest_b e2e load test: {} absent",
                path.display()
            );
            return;
        }

        let engine = test_engine();
        let instance = load_artifact(&engine, "pintest_b")
            .expect("pintest_b should load through the wasmos-native install path");
        let _ = instance;
    }

    /// Arc 2 (behavior test) — the type-focused artifact. `typetest`
    /// exercises the full `duckdb:extension/types` surface: value
    /// crossings for every arm of `Duckvalue` (including the rich
    /// Decimal/Interval/Uuid/Hugeint/Complex payloads) and the
    /// `Logicaltype` variant with its S2 `Decimal(decimalshape)` +
    /// T2-1 `Hugeint`/`Uhugeint` first-class arms. Successful load
    /// validates that the wasmos-native `types` host trait handler
    /// classifies + marshals every arm the mirror types cover.
    #[test]
    fn wasmos_native_load_typetest_end_to_end() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions/typetest.wasm");
        if !path.exists() {
            eprintln!(
                "skipping wasmos-native typetest e2e load test: {} absent",
                path.display()
            );
            return;
        }

        let engine = test_engine();
        let instance = load_artifact(&engine, "typetest")
            .expect("typetest should load through the wasmos-native install path");
        let _ = instance;
    }

    /// Arc 2 (behavior test) — a real production component
    /// (`atbash`, the letter-substitution cipher extension) loaded
    /// via the wasmos-native install path. Unlike the pintest / typetest
    /// artifacts (which are shaped specifically for host-side
    /// exercise), `atbash` is a live extension consumed by real users.
    /// Successful load through the wasmos path validates that the
    /// migration is production-quality on shapes that weren't authored
    /// with the tests in mind.
    #[test]
    fn wasmos_native_load_atbash_production_component() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions/atbash.wasm");
        if !path.exists() {
            eprintln!(
                "skipping wasmos-native atbash e2e load test: {} absent",
                path.display()
            );
            return;
        }

        let engine = test_engine();
        let instance = load_artifact(&engine, "atbash")
            .expect("atbash should load through the wasmos-native install path");
        let _ = instance;
    }

    /// Arc 2 (behavior test) — verifies the load path is
    /// re-entrant. Load the same artifact twice on the same engine
    /// and assert that both loads succeed independently. Guards
    /// against a class of bug where the wasmos install path caches
    /// per-Component state that would prevent a second load (or
    /// leaks state from a prior load into a fresh instance).
    #[test]
    fn wasmos_native_load_reentrant_on_same_engine() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions/pintest_a.wasm");
        if !path.exists() {
            eprintln!("skipping re-entrant load test: {} absent", path.display());
            return;
        }
        let engine = test_engine();
        // First load.
        let first = load_artifact(&engine, "pintest_a")
            .expect("first load should succeed");
        drop(first);
        // Second load on the same engine — must succeed independently.
        let second = load_artifact(&engine, "pintest_a")
            .expect("second load on same engine should succeed independently");
        drop(second);
    }

    #[test]
    fn convert_logicaltype_covers_every_arm_incl_rich_and_complex() {
        use Logicaltype as L;
        assert_eq!(
            convert_extension_logicaltype(L::Boolean),
            reg::LogicalType::Boolean
        );
        assert_eq!(
            convert_extension_logicaltype(L::Int8),
            reg::LogicalType::Int8
        );
        assert_eq!(
            convert_extension_logicaltype(L::Uint32),
            reg::LogicalType::Uint32
        );
        assert_eq!(
            convert_extension_logicaltype(L::Timestamptz),
            reg::LogicalType::Timestamptz
        );
        assert_eq!(
            convert_extension_logicaltype(L::Uuid),
            reg::LogicalType::Uuid
        );
        // The escape-hatch Complex arm carries its owned type-expr through.
        let cx = convert_extension_logicaltype(L::Complex("INTEGER[]".to_string()));
        assert_eq!(cx, reg::LogicalType::Complex("INTEGER[]".to_string()));
        assert_eq!(cx.describe(), "INTEGER[]");
        // Every arm converts without panicking and yields a non-empty label.
        for ty in all_ext_logicaltypes() {
            assert!(!convert_extension_logicaltype(ty).describe().is_empty());
        }
    }

    #[test]
    fn convert_funcargs_preserves_names_and_types() {
        use Logicaltype as L;
        let args = vec![
            Funcarg { name: Some("x".to_string()), logical: L::Int64 },
            Funcarg { name: None, logical: L::Text },
        ];
        let out = convert_extension_funcargs(args);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name.as_deref(), Some("x"));
        assert_eq!(out[0].logical, reg::LogicalType::Int64);
        assert_eq!(out[1].name, None);
        assert_eq!(out[1].logical, reg::LogicalType::Text);
    }

    #[test]
    fn convert_columndefs_preserves_names_and_types() {
        use Logicaltype as L;
        let cols = vec![
            Columndef { name: "id".to_string(), logical: L::Int32 },
            Columndef { name: "label".to_string(), logical: L::Complex("VARCHAR[]".to_string()) },
        ];
        let out = convert_extension_columndefs(cols);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "id");
        assert_eq!(out[0].logical, reg::LogicalType::Int32);
        assert_eq!(
            out[1].logical,
            reg::LogicalType::Complex("VARCHAR[]".to_string())
        );
    }

    #[test]
    fn convert_funcflags_maps_each_bit() {
        let none = convert_extension_funcflags(Funcflags::empty());
        assert_eq!(none, reg::FuncFlags::default());
        let all = convert_extension_funcflags(
            Funcflags::DETERMINISTIC
                | Funcflags::COMMUTATIVE
                | Funcflags::STATELESS
                | Funcflags::SIDEEFFECTING
                | Funcflags::DEPRECATED,
        );
        assert!(all.deterministic && all.commutative && all.stateless);
        assert!(all.side_effecting && all.deprecated);
        let det = convert_extension_funcflags(Funcflags::DETERMINISTIC);
        assert!(det.deterministic);
        assert!(!det.commutative && !det.stateless && !det.side_effecting && !det.deprecated);
    }

    // Phase 6.2.j — the following test blocks retired with the
    // wit-bindgen dispatch path they exercised:
    //   * storage_duckvalue_converts_every_arm_incl_rich
    //   * storage_logicaltype_and_columndef_convert_every_arm
    //   * storage_and_index_duckerror_map_every_arm
    // The `storage_duckvalue_to_ext` helper still lives (one production
    // callsite in `storage_scan_open`); its 25-arm coverage is now
    // exercised through the integration path rather than a unit test.
    // The other converters (logicaltype/columndef/duckerror for both
    // storage and index) are gone entirely — their targets went with
    // the export_marshal migration.

    // Phase 6.2.l.2 — the `configerror_and_loglevel_converters_
    // cover_arms` test retired here. It exercised the
    // `neutral_configerror_to_ext` / `ext_loglevel_to_neutral`
    // helpers that fed the retired `extension_config::Host` and
    // `extension_logging::Host` impls; the wasmos-native ConfigHost
    // / LoggingHost in extension_wasmos.rs carry their own
    // converters covering the same arms.

    // --- capture-into-pending logic (Host trait impls) ---
    //
    // Phase 6.2.l.2 — the following tests retired here:
    //   * register_collation_returns_unsupported
    //   * register_index_type_returns_unsupported
    //   * the `register_files → Unsupported` half of
    //     register_storage_captures_files_returns_unsupported
    // Their behavior is exercised on the wasmos-native production
    // path via extension_wasmos.rs tests (collation_returns_
    // unsupported / index_returns_unsupported / files_reg_returns_
    // unsupported), and the corresponding Host trait impls in the
    // extension.rs body retired in the same pass. The
    // register_storage capture-into-pending half survives below
    // (renamed) — that path IS unique-value coverage.

    #[test]
    fn register_storage_captures_into_pending() {
        // Phase 2 (@5): register_storage CAPTURES into pending_storages
        // (drained into `ExtensionManager::storage_backends`) rather than
        // rejecting as Unsupported. The host's ATTACH intercept dispatches
        // through the captured mapping. Phase 6.2.l.2 dropped the paired
        // `register_files → Unsupported` assertion (redundant with the
        // wasmos-side `files_reg_returns_unsupported` test).
        let mut state = test_state();
        let storage_res = state.register_storage("sqlitewasm".to_string(),
            7,
            None,
        );
        assert_eq!(storage_res.ok(), Some(7));
        let storages = state.take_pending_storages();
        assert_eq!(storages.len(), 1);
        assert_eq!(storages[0].type_name, "sqlitewasm");
        assert_eq!(storages[0].callback_handle, 7);
    }

    #[test]
    fn register_logical_type_and_macro_capture_into_pending() {
        let mut state = test_state();
        state
            .register_logical_type("myint".to_string(), "INTEGER".to_string())
            .expect("register_logical_type should not error");
        state
            .register_macro(
                "main".to_string(),
                "addone".to_string(),
                vec!["x".to_string()],
                "x + 1".to_string(),
            )
            .expect("register_macro should not error");
        let drained = state.drain_pending();
        assert_eq!(drained.logical_types.len(), 1);
        assert_eq!(drained.logical_types[0].name, "myint");
        assert_eq!(drained.macros.len(), 1);
        assert_eq!(drained.macros[0].name, "addone");
        assert_eq!(drained.macros[0].parameters, vec!["x".to_string()]);
    }

    #[test]
    fn register_copy_handler_captures_into_pending() {
        // 2.1.0 (Item 1): copy handlers are now CAPTURED (driven through
        // copy-dispatch), not rejected. Registration succeeds and lands in the
        // neutral pending buffer with the routing function-handle preserved.
        let mut state = test_state();
        let res = state.register_copy_handler("parquet".to_string(), 7);
        assert!(res.is_ok());
        let captured = state.take_pending_copy_handlers();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].file_extension, "parquet");
        assert_eq!(captured[0].function_handle, 7);
    }

    #[test]
    fn registers_2_1_0_additive_capabilities_into_pending() {
        // Phase 3 (@5 host-import wiring): secret type + provider now CAPTURE
        // into `pending_secrets` (drained into `ExtensionManager::secret_backends`
        // by the host) instead of returning Unsupported; settings option, table
        // macro, modified logical type, and enum also capture as before.
        let mut state = test_state();

        let secret_type_res = state.register_secret_type(
            "s3".to_string(),
            vec![("key_id".to_string(), false), ("secret".to_string(), true)],
            11,
        );
        assert!(
            secret_type_res.is_ok(),
            "register_secret_type should capture (Phase 3)"
        );
        let secret_provider_res = state.register_secret_provider("s3".to_string(),
            "credential_chain".to_string(),
            12,
        );
        assert!(
            secret_provider_res.is_ok(),
            "register_secret_provider should capture (Phase 3)"
        );

        state
            .register_option(
                "my_threshold".to_string(),
                "tuning knob".to_string(),
                "bigint".to_string(),
                Some("42".to_string()),
                "global".to_string(),
            )
            .expect("register_option");

        state
            .register_table_macro(
                "main".to_string(),
                "series".to_string(),
                vec!["n".to_string()],
                "SELECT * FROM range(n)".to_string(),
            )
            .expect("register_table_macro");

        state
            .register_logical_type_modified("price".to_string(), "DECIMAL(18,3)".to_string())
            .expect("register_logical_type_modified");
        state
            .register_enum("mood".to_string(), vec!["happy".to_string(), "sad".to_string()])
            .expect("register_enum");

        // Phase 3: both register_secret_* calls capture into pending_secrets.
        // The type registration has params + `provider = None`; the provider
        // registration carries `Some(provider)` and no params.
        let secrets = state.take_pending_secrets();
        assert_eq!(secrets.len(), 2);
        let (type_regs, provider_regs): (Vec<_>, Vec<_>) =
            secrets.iter().partition(|s| s.provider.is_none());
        assert_eq!(type_regs.len(), 1);
        assert_eq!(type_regs[0].type_name, "s3");
        assert_eq!(type_regs[0].callback_handle, 11);
        assert_eq!(type_regs[0].params.len(), 2);
        assert_eq!(type_regs[0].params[0], ("key_id".to_string(), false));
        assert_eq!(type_regs[0].params[1], ("secret".to_string(), true));
        assert_eq!(provider_regs.len(), 1);
        assert_eq!(provider_regs[0].type_name, "s3");
        assert_eq!(
            provider_regs[0].provider.as_deref(),
            Some("credential_chain")
        );
        assert_eq!(provider_regs[0].callback_handle, 12);
        assert!(provider_regs[0].params.is_empty());

        let settings = state.take_pending_settings();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[0].name, "my_threshold");
        assert_eq!(settings[0].ty, "bigint");
        assert_eq!(settings[0].scope, "global");
        assert_eq!(settings[0].default_value.as_deref(), Some("42"));

        let macros = state.take_pending_table_macros();
        assert_eq!(macros.len(), 1);
        assert_eq!(macros[0].name, "series");

        let modified = state.take_pending_modified_types();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].type_expr, "DECIMAL(18,3)");

        let enums = state.take_pending_enum_types();
        assert_eq!(enums.len(), 1);
        assert_eq!(
            enums[0].members,
            vec!["happy".to_string(), "sad".to_string()]
        );
    }

    #[test]
    fn registers_2_2_0_additive_capabilities_into_pending() {
        // 2.2.0 (Items 6-7): the richer scalar (scalar-ex), connection-lifecycle
        // subscription, coordinate system, Arrow table, text encoding, and
        // compression codec all CAPTURE into their neutral pending buffers.
        use Logicaltype as L;
        let mut state = test_state();

        // Item 6: register-scalar-ex with varargs + special NULL handling.
        state
            .register_scalar_ex(
                "concat_ws".to_string(),
                vec![Funcarg { name: Some("sep".to_string()), logical: L::Text }],
                Some(L::Text),
                L::Text,
                true, // special_null
                21,
                None,
            )
            .expect("register_scalar_ex");

        // Phase 6.2.l.2 — the register_connection_callback →
        // Unsupported assertion retired here (redundant with the
        // wasmos-side `register_connection_callback_returns_
        // unsupported` test). Its Host impl retired too.

        // Item 7: coordinate system.
        state
            .register_coordinate_system("EPSG".to_string(), 4326, "GEOGCRS[...]".to_string())
            .expect("register_coordinate_system");

        // Item 7: Arrow table producer.
        state
            .register_arrow_table(
                "feed".to_string(),
                vec![Columndef { name: "v".to_string(), logical: L::Int64 }],
                23,
            )
            .expect("register_arrow_table");

        // Phase 6.2.l.2 — register_encoding + register_compression
        // → Unsupported assertions retired here (redundant with
        // wasmos-side `encoding_returns_unsupported` /
        // `compression_returns_unsupported`). Their Host impls
        // retired too.

        let scalar_ex = state.take_pending_scalar_ex();
        assert_eq!(scalar_ex.len(), 1);
        assert_eq!(scalar_ex[0].name, "concat_ws");
        assert_eq!(scalar_ex[0].extension, "testext");
        assert!(
            scalar_ex[0].special_null,
            "special NULL handling must be captured"
        );
        assert_eq!(scalar_ex[0].varargs, Some(reg::LogicalType::Text));
        assert_eq!(scalar_ex[0].callback_handle, 21);

        let crs = state.take_pending_coordinate_systems();
        assert_eq!(crs.len(), 1);
        assert_eq!(crs[0].auth_name, "EPSG");
        assert_eq!(crs[0].code, 4326);

        let arrow = state.take_pending_arrow_tables();
        assert_eq!(arrow.len(), 1);
        assert_eq!(arrow[0].name, "feed");
        assert_eq!(arrow[0].columns.len(), 1);
        assert_eq!(arrow[0].callback_handle, 23);

    }

    #[test]
    fn nested_value_rides_complex_arm_without_new_type_arm() {
        // Nested LIST/STRUCT values ride the EXISTING `complex(type-expr, json)`
        // escape hatch on `duckvalue` -- no new `duckvalue`/`logicaltype` arm, so
        // the bump stays additive (2.1.0). This asserts a flat-encoded LIST value
        // is carried through the base types verbatim (the CORE reconstructs the
        // real LIST vector from the type-expr + JSON via the duckdb C vector API).
        let v = Duckvalue::Complex(Complexvalue {
            type_expr: "INTEGER[]".to_string(),
            json: "[10,20,30]".to_string(),
        });
        match v {
            Duckvalue::Complex(c) => {
                assert_eq!(c.type_expr, "INTEGER[]");
                assert_eq!(c.json, "[10,20,30]");
            }
            _ => panic!("expected complex arm"),
        }
    }

    #[test]
    fn replacement_scan_unknown_table_handle_errors_not_panics() {
        let mut state = test_state();
        // No table function was ever registered, so handle 999 is unknown: the
        // capture must return Err, not panic.
        let res = state.register_replacement_scan(999, vec!["csv".to_string()], "ExtensionOnly");
        assert!(res.is_err());
    }

    #[test]
    fn register_pragma_with_unknown_callback_handle_errors_not_panics() {
        let mut state = test_state();
        // A pragma callback handle that was never registered in the callback
        // registry -> Err, not a panic. Registry rep is arbitrary.
        let res = state.register_call(1, "my_pragma".to_string(), 424242);
        assert!(res.is_err());
    }

    #[test]
    fn drain_pending_is_empty_on_fresh_state() {
        let mut state = test_state();
        let drained = state.drain_pending();
        assert!(drained.scalars.is_empty());
        assert!(drained.tables.is_empty());
        assert!(drained.aggregates.is_empty());
        assert!(drained.macros.is_empty());
        assert!(drained.logical_types.is_empty());
    }

    #[test]
    fn summarize_registration_names_truncates_with_more() {
        let names = ["a", "b", "c", "d", "e"];
        let s = summarize_registration_names(&names, |n| n);
        assert!(s.contains('a'));
        assert!(s.contains("+2 more"));
        assert_eq!(summarize_registration_names::<&str, _>(&[], |n| n), "none");
    }

    // ------------------------------------------------------------------
    // nested-exec tests
    // ------------------------------------------------------------------

    /// A services sink that answers `nested_exec` from an in-memory script,
    /// returning canned rows for the first call, then `rows-affected` for the
    /// second. Used to prove the `Host` impl round-trips the neutral result
    /// into the WIT `Execresult` shape without touching a real database.
    struct ScriptedNestedServices {
        select_rows: Vec<Vec<String>>,
        dml_affected: u64,
        calls: u32,
    }

    impl ExtensionServices for ScriptedNestedServices {
        fn provider_version(&mut self) -> Result<String, ConfigError> {
            Ok("test".to_string())
        }
        fn list_keys(&mut self, _prefix: Option<&str>) -> Result<Vec<String>, ConfigError> {
            Ok(Vec::new())
        }
        fn get_string(&mut self, _path: &str) -> Result<Option<String>, ConfigError> {
            Ok(None)
        }
        fn get_bool(&mut self, _path: &str) -> Result<Option<bool>, ConfigError> {
            Ok(None)
        }
        fn get_i64(&mut self, _path: &str) -> Result<Option<i64>, ConfigError> {
            Ok(None)
        }
        fn get_u64(&mut self, _path: &str) -> Result<Option<u64>, ConfigError> {
            Ok(None)
        }
        fn get_f64(&mut self, _path: &str) -> Result<Option<f64>, ConfigError> {
            Ok(None)
        }
        fn get_bytes(&mut self, _path: &str) -> Result<Option<Vec<u8>>, ConfigError> {
            Ok(None)
        }
        fn get_string_list(&mut self, _path: &str) -> Result<Option<Vec<String>>, ConfigError> {
            Ok(None)
        }
        fn log(&mut self, _level: LogLevel, _message: &str, _target: Option<&str>) {}
        fn log_fields(&mut self, _level: LogLevel, _message: &str, _fields: &[LogField]) {}

        fn nested_exec(&mut self, _sql: &str) -> Result<NestedExecResult, String> {
            self.calls += 1;
            if self.calls == 1 {
                Ok(NestedExecResult {
                    rows: Some(self.select_rows.clone()),
                    rows_affected: None,
                })
            } else {
                Ok(NestedExecResult {
                    rows: None,
                    rows_affected: Some(self.dml_affected),
                })
            }
        }
    }

    fn scripted_state() -> ExtensionStoreState {
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
        ExtensionStoreState::new(
            wasi,
            Box::new(ScriptedNestedServices {
                select_rows: vec![
                    vec!["1".to_string(), "alpha".to_string()],
                    vec!["2".to_string(), String::new()], // NULL rendered as ""
                ],
                dml_affected: 7,
                calls: 0,
            }),
            Arc::new(RwLock::new(CallbackRegistry::default())),
            "testext".to_string(),
        )
    }

    #[test]
    fn nested_exec_select_returns_rows() {
        let mut state = scripted_state();
        let r = state.nested_exec("SELECT * FROM t".to_string())
            .expect("select ok");
        let rows = r.rows.expect("SELECT populates rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].as_str(), "1");
        assert_eq!(rows[0][1].as_str(), "alpha");
        assert_eq!(rows[1][1].as_str(), ""); // NULL round-trip
        assert!(r.rows_affected.is_none());
    }

    #[test]
    fn nested_exec_dml_returns_rows_affected() {
        let mut state = scripted_state();
        // First call = SELECT (drains the script's initial arm).
        let _ = state.nested_exec("SELECT 1".to_string())
            .expect("select ok");
        // Second call = DML: rows None, rows_affected Some.
        let r = state.nested_exec("INSERT INTO t VALUES (3, 'gamma')".to_string(),
        )
        .expect("insert ok");
        assert!(r.rows.is_none());
        assert_eq!(r.rows_affected, Some(7));
    }

    #[test]
    fn nested_exec_depth_cap_returns_error_at_level_max_plus_one() {
        // Manually pre-bump the per-thread counter to the ceiling; the next
        // Host::nested_exec call must fail without ever calling into the sink.
        NESTED_EXEC_DEPTH.with(|d| d.set(NESTED_EXEC_MAX_DEPTH));
        let mut state = scripted_state();
        let err = state.nested_exec("SELECT 1".to_string())
            .expect_err("depth-cap error");
        assert!(
            err.contains("max nesting depth"),
            "unexpected depth-cap error: {err}"
        );
        // Sink must NOT have been called.
        // (calls stays at 0 because we short-circuited before the sink.)
        // Restore the counter for other tests running on this thread.
        NESTED_EXEC_DEPTH.with(|d| d.set(0));
    }

    #[test]
    fn nested_exec_depth_guard_decrements_on_drop() {
        NESTED_EXEC_DEPTH.with(|d| d.set(0));
        {
            let _g1 = NestedExecDepthGuard::enter().expect("depth 0->1");
            NESTED_EXEC_DEPTH.with(|d| assert_eq!(d.get(), 1));
            {
                let _g2 = NestedExecDepthGuard::enter().expect("depth 1->2");
                NESTED_EXEC_DEPTH.with(|d| assert_eq!(d.get(), 2));
            }
            NESTED_EXEC_DEPTH.with(|d| assert_eq!(d.get(), 1));
        }
        NESTED_EXEC_DEPTH.with(|d| assert_eq!(d.get(), 0));
    }

    // ------------------------------------------------------------------
    // file-lock tests
    // ------------------------------------------------------------------

    /// Produce a unique per-test-invocation lock path in the OS temp dir so
    /// parallel `cargo test` invocations never fight over the same file.
    fn unique_lock_path(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ducklink-runtime-file-lock-{tag}-{pid}-{stamp}.lock"
        ));
        p
    }

    #[test]
    fn file_lock_try_acquire_returns_none_when_held() {
        let path = unique_lock_path("try");
        let held =
            LockHandleState::acquire_exclusive(path.to_str().unwrap()).expect("first acquire");
        let contested = LockHandleState::try_acquire_exclusive(path.to_str().unwrap())
            .expect("try-acquire IO ok");
        assert!(
            contested.is_none(),
            "try-acquire on a held lock must return None (would-block)"
        );
        drop(held);
        // After release, try-acquire succeeds again.
        let after = LockHandleState::try_acquire_exclusive(path.to_str().unwrap())
            .expect("try-acquire IO ok after release")
            .expect("try-acquire returns Some once free");
        drop(after);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_lock_race_between_threads_serializes() {
        // Four threads race to acquire-exclusive on the SAME lock file. Each
        // acquires, bumps a shared counter to prove it holds alone, sleeps
        // briefly, decrements, then releases. If flock is honored, the
        // counter is never > 1 at any observation point; if it isn't, at
        // least one thread will see counter >= 2 during its critical section.
        // We also verify every thread eventually succeeded and total
        // successful acquisitions == thread count.
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::sync::Arc;
        use std::thread;

        let path = unique_lock_path("race");
        let path_str = path.to_string_lossy().into_owned();
        let held_count = Arc::new(AtomicI32::new(0));
        let max_seen = Arc::new(AtomicI32::new(0));
        let successes = Arc::new(AtomicI32::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path_str = path_str.clone();
                let held_count = held_count.clone();
                let max_seen = max_seen.clone();
                let successes = successes.clone();
                thread::spawn(move || {
                    // Acquiring here blocks until the previous holder drops.
                    let lock = LockHandleState::acquire_exclusive(&path_str).expect("acquire ok");
                    let now = held_count.fetch_add(1, Ordering::AcqRel) + 1;
                    // Track the max ever observed inside a held section.
                    let mut cur_max = max_seen.load(Ordering::Acquire);
                    while now > cur_max {
                        match max_seen.compare_exchange(
                            cur_max,
                            now,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break,
                            Err(observed) => cur_max = observed,
                        }
                    }
                    // Yield the CPU to give any racing thread a chance to
                    // see us inside the section (this AMPLIFIES a broken
                    // impl; the sleep is not correctness-critical).
                    thread::sleep(std::time::Duration::from_millis(20));
                    held_count.fetch_sub(1, Ordering::AcqRel);
                    successes.fetch_add(1, Ordering::AcqRel);
                    drop(lock);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread join ok");
        }

        assert_eq!(
            max_seen.load(Ordering::Acquire),
            1,
            "flock is broken: multiple threads observed inside the section"
        );
        assert_eq!(
            successes.load(Ordering::Acquire),
            4,
            "every thread must eventually acquire"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Phase 6.2.n Session 2 — `file_lock_host_acquire_and_release_
    // round_trip` retired here (it drove the test-only inherent
    // methods that mirrored the wit-bindgen HostLockHandle path).
    // The wasmos-native `file_lock_acquire_then_release_removes_
    // from_state` + `file_lock_acquire_then_on_resource_drop_
    // removes_from_state` tests in extension_wasmos::tests cover
    // the same acquire → release → drop behavior on the production
    // dispatch path.
}

/// Process-global cache for the base [`Linker`] template — the one populated
/// by [`add_extension_interfaces_to_linker`]. Built lazily on the first load
/// and cloned on every subsequent load, so the ~25 `add_to_linker` calls the
/// linker construction requires run ONCE per process instead of once per
/// component load. Guarded by an [`Engine`] identity check so a hypothetical
/// second Engine gets its own fresh linker rather than incorrectly reusing
/// one bound to a different engine.
static BASE_LINKER_CACHE: OnceLock<Mutex<Option<(Engine, Linker<ExtensionStoreState>)>>> =
    OnceLock::new();

/// Return a `Linker<ExtensionStoreState>` populated with the base extension
/// interfaces for `engine`. First call runs
/// [`add_extension_interfaces_to_linker`]; subsequent calls (with the same
/// engine) clone the cached template. A different engine falls back to a
/// fresh build.
fn base_linker(engine: &Engine) -> wasmtime::Result<Linker<ExtensionStoreState>> {
    let cell = BASE_LINKER_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_engine, cached_linker)) = guard.as_ref() {
        if Engine::same(cached_engine, engine) {
            return Ok(cached_linker.clone());
        }
    }
    let mut linker = Linker::<ExtensionStoreState>::new(engine);
    add_extension_interfaces_to_linker(&mut linker)?;
    *guard = Some((engine.clone(), linker.clone()));
    Ok(linker)
}

/// Add the full `duckdb:extension` capability surface to `linker`: the wasip2
/// preview interfaces (so the component's WASI imports resolve) plus all six
/// extension interfaces (types, runtime, config, logging, catalog, files), each
/// dispatched to the `ExtensionStoreState`. Used by both directions before
/// instantiating a component.
pub fn add_extension_interfaces_to_linker(
    linker: &mut Linker<ExtensionStoreState>,
) -> wasmtime::Result<()> {
    wasmtime_wasi::p2::add_to_linker_sync(linker)?;
    // wasi:http/{types,outgoing-handler}@0.2.9 (see the WasiHttpCtx doc on
    // ExtensionStoreState). `add_only_http_to_linker_sync` — NOT the full
    // `add_to_linker_sync` — because the wasi:cli / wasi:filesystem / etc.
    // interfaces are already added by the wasmtime_wasi call above; the full
    // wasi:http `add_to_linker_sync` re-adds the wasi:http/proxy world and
    // would collide.
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(linker)?;
    // ADR-0029 Phase 6.2.h — every `duckdb:extension/*` host import
    // used to be wired here through wit-bindgen's `extension_xxx::
    // add_to_linker`. The full 27-interface sweep is now on the
    // wasmos install path (`install_wasmos_migrated_interfaces`) —
    // see the phase-by-phase attribution in the git log (Phase
    // 6.2.h.2 through 6.2.h.7) or the state-of-the-abstraction doc.
    // The `Host` trait impls immediately below survive as test
    // scaffolding for register/lifecycle unit tests but no linker
    // path invokes them any more. Phase 6.2.l retired the last of
    // the commented-out `add_to_linker` calls that used to sit here.
    Ok(())
}

/// ADR-0029 Phase 6.2.h.2 — wire the wasmos-migrated interfaces on
/// `linker` for `component`. Called per-load from
/// [`load_component_with_dynlink`] because the wasmos
/// `install_stateless_host_call` bridge needs the [`Component`] in scope
/// to enumerate method signatures at wire time (the wit-bindgen
/// `add_to_linker` shape doesn't; the interface shape is compile-time-
/// known via bindgen).
///
/// Interfaces migrated so far:
///
/// - `duckdb:extension/lifecycle` — Phase 6.2.h.2. 1 method.
/// - `duckdb:extension/types` — Phase 6.2.h.3. 0 methods (marker).
/// - `duckdb:extension/encoding` — Phase 6.2.h.3. 1 method.
/// - `duckdb:extension/compression` — Phase 6.2.h.3. 1 method.
/// - `duckdb:extension/files-reg` — Phase 6.2.h.3. 1 method.
/// - `duckdb:extension/index` — Phase 6.2.h.3. 1 method.
/// - `duckdb:extension/collation` — Phase 6.2.h.3. 1 method.
///
/// All 7 are stateless (no `SharedExtensionState` — fresh instance per
/// load is safe) and resource-free (no `Resource<T>` in any method
/// signature). The `#[host_iface(sync)]`-emitted `impl SyncHostCall`
/// on each host struct provides the dispatch entry point.
///
/// Future sessions add the resource-aware bridge for the
/// Resource<T>-carrying interfaces (Runtime, FileLock, Files, Catalog,
/// ...).
///
/// **Non-blocking behaviour**: if `component` doesn't import a given
/// migrated interface, the bridge no-ops for that interface — matches
/// the wasmos `wire_host_imports` policy so a shared installer works
/// across a mixed extension set.
fn install_wasmos_migrated_interfaces(
    engine: &Engine,
    linker: &mut Linker<ExtensionStoreState>,
    component: &Component,
) -> wasmtime::Result<()> {
    use std::sync::Arc as StdArc;
    use wasmos_runtime_api::SyncHostCall as _SyncHostCall;
    use wasmos_runtime_wasmtime_v48::sync_bridge::install_stateless_host_call;

    // Table of (iface-name, handler-factory) pairs, one row per
    // migrated interface. Keeps the wiring uniform — every additional
    // stateless-no-resource interface migrates as one new entry, and
    // the loop enforces the same error-mapping shape for each.
    //
    // The handler-factory closure returns Arc<dyn SyncHostCall>; each
    // returns a fresh instance since these hosts are stateless.
    let migrated: [(&str, fn() -> StdArc<dyn _SyncHostCall>); 7] = [
        // Phase 6.2.h.2 — Session 2 first migration.
        (
            "duckdb:extension/lifecycle@5.0.0",
            || StdArc::new(crate::extension_wasmos::LifecycleHost::new()),
        ),
        // Phase 6.2.h.3 — the six remaining stateless interfaces.
        (
            "duckdb:extension/types@5.0.0",
            || StdArc::new(crate::extension_wasmos::TypesHost::new()),
        ),
        (
            "duckdb:extension/encoding@5.0.0",
            || StdArc::new(crate::extension_wasmos::EncodingHost::new()),
        ),
        (
            "duckdb:extension/compression@5.0.0",
            || StdArc::new(crate::extension_wasmos::CompressionHost::new()),
        ),
        (
            "duckdb:extension/files-reg@5.0.0",
            || StdArc::new(crate::extension_wasmos::FilesRegHost::new()),
        ),
        (
            "duckdb:extension/index@5.0.0",
            || StdArc::new(crate::extension_wasmos::IndexHost::new()),
        ),
        (
            "duckdb:extension/collation@5.0.0",
            || StdArc::new(crate::extension_wasmos::CollationHost::new()),
        ),
    ];

    for (iface, factory) in migrated {
        install_stateless_host_call(engine, linker, component, iface, factory())
            .map_err(|e| wasmtime::Error::msg(format!(
                "install_stateless_host_call({iface}) failed: {e}"
            )))?;
    }

    // ADR-0029 Phase 6.2.h.5 + h.6 — resource-aware bridge
    // migrations. Every stateful-plus-resource-free interface
    // wires here via `install_host_call`, using `XxxHost::bridged()`
    // — the StateSource::FromCtx constructor. The bridge populates
    // `HostCallContext::consumer_state` per-call with
    // `store.data_mut()`; each handler's `self.state.hold(ctx)?`
    // pulls the state's mutable reference from ctx and operates on
    // it directly — no divergent state instance.
    //
    // File_lock IS the sole resource-carrying interface in this
    // table (1 resource: `lock-handle`); every other entry has
    // zero resources. `install_host_call` handles both cleanly: for
    // zero-resource interfaces the resource-discs map stays empty
    // and no resource marshal happens.
    //
    // Runtime (10 resources, multi-resource) DEFERRED to
    // Phase 6.2.h.7 pending per-return-type discriminant
    // classification on the bridge — the single-`sole_disc`
    // fallback only covers 0-1 resource interfaces.
    let stateful_migrated: [(&str, fn() -> StdArc<dyn _SyncHostCall>); 20] = [
        // File-lock — 1 resource, migrated Phase 6.2.h.5.
        (
            "duckdb:extension/file-lock@5.0.0",
            || StdArc::new(crate::extension_wasmos::FileLockHost::bridged()),
        ),
        // Resource-free stateful interfaces — Phase 6.2.h.6.
        (
            "duckdb:extension/config@5.0.0",
            || StdArc::new(crate::extension_wasmos::ConfigHost::bridged()),
        ),
        (
            "duckdb:extension/logging@5.0.0",
            || StdArc::new(crate::extension_wasmos::LoggingHost::bridged()),
        ),
        (
            "duckdb:extension/catalog@5.0.0",
            || StdArc::new(crate::extension_wasmos::CatalogHost::bridged()),
        ),
        (
            "duckdb:extension/files@5.0.0",
            || StdArc::new(crate::extension_wasmos::FilesHost::bridged()),
        ),
        (
            "duckdb:extension/storage@5.0.0",
            || StdArc::new(crate::extension_wasmos::StorageHost::bridged()),
        ),
        (
            "duckdb:extension/query@5.0.0",
            || StdArc::new(crate::extension_wasmos::QueryHost::bridged()),
        ),
        (
            "duckdb:extension/nested-exec@5.0.0",
            || StdArc::new(crate::extension_wasmos::NestedExecHost::bridged()),
        ),
        (
            "duckdb:extension/secret@5.0.0",
            || StdArc::new(crate::extension_wasmos::SecretHost::bridged()),
        ),
        (
            "duckdb:extension/settings@5.0.0",
            || StdArc::new(crate::extension_wasmos::SettingsHost::bridged()),
        ),
        (
            "duckdb:extension/macro-ext@5.0.0",
            || StdArc::new(crate::extension_wasmos::MacroExtHost::bridged()),
        ),
        (
            "duckdb:extension/types-ext@5.0.0",
            || StdArc::new(crate::extension_wasmos::TypesExtHost::bridged()),
        ),
        (
            "duckdb:extension/runtime-ext@5.0.0",
            || StdArc::new(crate::extension_wasmos::RuntimeExtHost::bridged()),
        ),
        (
            "duckdb:extension/coordinate-system@5.0.0",
            || StdArc::new(crate::extension_wasmos::CoordinateSystemHost::bridged()),
        ),
        (
            "duckdb:extension/arrow-ext@5.0.0",
            || StdArc::new(crate::extension_wasmos::ArrowExtHost::bridged()),
        ),
        (
            "duckdb:extension/parser@5.0.0",
            || StdArc::new(crate::extension_wasmos::ParserHost::bridged()),
        ),
        (
            "duckdb:extension/optimizer@5.0.0",
            || StdArc::new(crate::extension_wasmos::OptimizerHost::bridged()),
        ),
        (
            "duckdb:extension/table-stream@5.0.0",
            || StdArc::new(crate::extension_wasmos::TableStreamHost::bridged()),
        ),
        (
            "duckdb:extension/log-storage@5.0.0",
            || StdArc::new(crate::extension_wasmos::LogStorageHost::bridged()),
        ),
        // ADR-0029 Phase 6.2.h.7 — Runtime, the final interface.
        // 10 resource types (5 XxxCallback + 4 XxxRegistry + macro-
        // registry) + the get-capability variant with 5 Resource-
        // carrying arms. Enabled by Phase 6.2.h.7's multi-resource
        // classification on the bridge — every Value::Resource now
        // carries its resource_name in the ctx's name_map, so lower
        // resolves the correct wasmtime discriminant per return.
        (
            "duckdb:extension/runtime@5.0.0",
            || StdArc::new(crate::extension_wasmos::RuntimeHost::bridged()),
        ),
    ];

    for (iface, factory) in stateful_migrated {
        wasmos_runtime_wasmtime_v48::sync_bridge_resource::install_host_call(
            engine,
            linker,
            component,
            iface,
            factory(),
        )
        .map_err(|e| wasmtime::Error::msg(format!(
            "install_host_call({iface}) failed: {e}"
        )))?;
    }

    Ok(())
}

/// Load a `duckdb:extension` component and run its `load()`, returning the
/// instantiated [`ExtensionInstance`] (which then holds the registrations the
/// component captured into its store-state via the `Host*` impls).
///
/// This is the direction-agnostic loader: the caller supplies the `wasi` context
/// (so it owns the sandbox/network policy) and the [`ExtensionServices`] sink
/// (so config/logging route to its database). Direction 1 (the wasm-DuckDB host)
/// and Direction 2 (the native-DuckDB extension) call this identically; only the
/// `services` they pass differ.
pub fn load_component(
    engine: &Engine,
    component: &Component,
    wasi: WasiCtx,
    services: Box<dyn ExtensionServices>,
    callback_registry: Arc<RwLock<CallbackRegistry>>,
    extension_name: String,
) -> wasmtime::Result<ExtensionInstance> {
    load_component_with_dynlink(
        engine,
        component,
        wasi,
        services,
        callback_registry,
        extension_name,
        None,
    )
}

/// Like [`load_component`] but also wires `compose:dynlink/linker` for a
/// component that imports it: the host import is added to the guest linker
/// (gated on `imports_linker`) and a [`DynLinkBridge`](crate::compose_dynlink::DynLinkBridge)
/// over the supplied shared provider `registry` is moved into the store
/// state. This is how an `ml_kmeans`-style aggregate reaches the one resident,
/// shared pylon provider. A component that does NOT import the linker (every
/// other extension) is unaffected even if a registry is supplied.
pub fn load_component_with_dynlink(
    engine: &Engine,
    component: &Component,
    wasi: WasiCtx,
    services: Box<dyn ExtensionServices>,
    callback_registry: Arc<RwLock<CallbackRegistry>>,
    extension_name: String,
    dynlink_registry: Option<crate::compose_dynlink::ProviderRegistry>,
) -> wasmtime::Result<ExtensionInstance> {
    // Contract guard: reject a component whose duckdb:extension contract major
    // differs from this host's (or is unversioned/legacy) BEFORE instantiating,
    // so a mismatched component never silently marshals corrupted values.
    crate::check_component_contract(engine, component, &extension_name)?;

    // H4: cache the fully-built base Linker (wasip2 + 24 duckdb:extension
    // interfaces) in a process-global OnceLock, keyed by Engine identity.
    // Every subsequent load clones the cached linker instead of running
    // ~25 `add_to_linker` calls. `Linker` is Clone and cheap.
    //
    // Different Engines are rejected: `Engine::same` compares refcounted
    // ids. In practice ducklink runs one Engine per process (Engine2::new
    // creates it once); a second Engine would hit the else arm and rebuild.
    let mut linker = base_linker(engine)?;

    // compose:dynlink/linker: conditionally satisfy a guest-driven provider
    // import. ONLY a component that actually imports the linker gets the host
    // import + a bridge; every other extension pays nothing (the gate mirrors
    // the framework's `imports_linker`).
    let dynlink = match dynlink_registry {
        Some(registry) if crate::compose_dynlink::imports_linker(engine, component) => {
            verbose_log!(
                "[extension-runtime:{extension_name}] imports compose:dynlink/linker; wiring the shared-provider bridge"
            );
            crate::compose_dynlink::add_to_linker::<ExtensionStoreState>(&mut linker)
                .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            Some(crate::compose_dynlink::new_resident(registry))
        }
        _ => None,
    };

    let mut store = Store::new(
        engine,
        ExtensionStoreState::with_dynlink(
            wasi,
            services,
            callback_registry,
            extension_name.clone(),
            dynlink,
        ),
    );

    // ADR-0029 Phase 6.2.h.2 — wire the wasmos-migrated interfaces
    // per-load. Session 2 covers Lifecycle only; future sessions add
    // the other stateless-and-no-resource interfaces (Types, Encoding,
    // Compression, FilesReg, Index, Collation) then face the resource-
    // marshalling design decision. Non-blocking for the current call:
    // if the component doesn't import lifecycle, the installer is a
    // no-op (matches wasmos wire_host_imports policy).
    install_wasmos_migrated_interfaces(engine, &mut linker, component)
        .map_err(|e| wasmtime::Error::msg(format!(
            "wasmos-native import wiring failed: {e}"
        )))?;

    // Instantiate via the linker to obtain the raw component instance, then build
    // the typed base-world bindings from it. Retaining the raw instance lets a
    // storage backend lazily build the storage-capable bindings later (the base
    // world doesn't mandate storage-dispatch, so non-storage extensions still
    // load here).
    let instance_pre = linker.instantiate_pre(component)?;
    let instance = instance_pre.instantiate(store.as_context_mut())?;
    // Phase 6.2.j — the wit-bindgen typed dispatcher construction
    // (`DuckdbExtension::new(store, &instance)?`) is gone. Every guest
    // export dispatches through the wasmos sync bridge using `instance`
    // directly; no typed dispatcher struct needed.

    // ADR-0029 Phase 6.2.i.3 — migrate `load()` from wit-bindgen's
    // typed dispatcher to the wasmos sync_export_bridge. First of
    // ~68 callsites; the rest migrate in follow-up sessions per the
    // Phase 6.2.i design brief.
    //
    // Wire equivalence: `bindings.duckdb_extension_guest().call_load(
    // store)` used to be a wit-bindgen macro-generated dispatcher
    // that looked up the "load" export inside the
    // `duckdb:extension/guest@5.0.0` interface, called it with no
    // args, and lifted the returned `result<loadresult, duckerror>`
    // to a typed Rust Result. The wasmos-native path does the same
    // via `call_export` with the qualified interface name + a
    // Value::Result match on the return.
    //
    // Version tag `@5.0.0` matches CONTRACT_MAJOR/MINOR — Phase
    // 6.2.h.8 established that wasmtime's Linker + Instance export
    // lookups match interface names verbatim including the version
    // suffix.
    let load_out = wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export(
        store.as_context_mut(),
        &instance,
        Some("duckdb:extension/guest@5.0.0"),
        "load",
        &[],
    )
    .map_err(|e| {
        wasmtime::Error::msg(format!(
            "extension component '{extension_name}' load() dispatch failed: {e}"
        ))
    })?;
    if load_out.len() != 1 {
        return Err(wasmtime::Error::msg(format!(
            "extension component '{extension_name}' load() returned {} values, expected 1",
            load_out.len()
        )));
    }
    match &load_out[0] {
        // Ok(loadresult) — success. The loadresult value is
        // opaque to the loader (it carries a version marker + any
        // additive fields), so we don't decode further here.
        wasmos_runtime_api::Value::Result(Ok(_)) => {}
        // Err(duckerror) — guest signalled a load failure. Preserve
        // the wit-bindgen counterpart's error message shape by
        // formatting the wasmos-native Duckerror value.
        wasmos_runtime_api::Value::Result(Err(payload)) => {
            return Err(wasmtime::Error::msg(format!(
                "extension component '{extension_name}' returned error from load(): {payload:?}"
            )));
        }
        other => {
            return Err(wasmtime::Error::msg(format!(
                "extension component '{extension_name}' load() returned unexpected \
                 non-Result value: {other:?}"
            )));
        }
    }

    Ok(ExtensionInstance::new(store, instance))
}
