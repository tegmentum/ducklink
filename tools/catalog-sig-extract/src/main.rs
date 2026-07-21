//! Enrich `registry/index.json` with a `functions` array per extension entry,
//! carrying full SQL signatures (argument types + return type / output columns).
//!
//! It loads each component with the SAME loader the native extension uses
//! (`ducklink_runtime::load_component`), drains the neutral registration model
//! (`reg::ScalarReg` / `TableReg` / `AggregateReg`), and maps each
//! `reg::LogicalType` to the DuckDB SQL type name that the native extension's
//! `src/reg_duckdb.rs` registers it as (so e.g. `Int64` -> `BIGINT`, not the raw
//! `INT64` describe() token). The `functions` field is ADDITIVE: every other
//! field of every entry is preserved byte-for-byte.
//!
//! Usage:
//!   catalog-sig-extract [--catalog <path>] [--out <sidecar.json>] [--report <path>]
//!   --catalog  path to registry/index.json (default: registry/index.json)
//!   --out      write the extracted `{name -> functions[]}` map to this JSON file
//!              (the sidecar `merge-functions.py` step folds it into the catalog
//!              byte-for-byte; default: dry-run, print coverage only)
//!   --report   write a coverage report (JSON) to this path
//!
//! NOTE: this tool ONLY EXTRACTS signatures (the part that needs to load wasm).
//! It deliberately does NOT serialize the catalog itself — serde_json emits
//! literal UTF-8 whereas the catalog is `ensure_ascii`-escaped, so the folding
//! is done by `merge-functions.py` (Python `json.dump(..., ensure_ascii=True)`)
//! to preserve every pre-existing byte. Keeping the two steps split is why the
//! additive `functions` key is the ONLY change to the catalog file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use ducklink_runtime::reg;
use ducklink_runtime::{
    load_component_with_dynlink, CallbackRegistry, ConfigError, ExtensionServices, LogField,
    LogLevel, PendingRegistrationsData, ProviderRegistry,
};
use serde_json::{json, Map, Value};
use wasmtime::component::Component;
use wasmtime::{Config, Engine};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

/// Map a neutral `reg::LogicalType` to the DuckDB SQL type name the native
/// extension registers it as. MIRRORS `src/reg_duckdb.rs`'s
/// `reg::LogicalType -> LogicalTypeId` mapping (BIGINT for Int64, DOUBLE for
/// Float64, etc.). `Decimal` registers as `LogicalTypeHandle::decimal(18, 3)`;
/// `Complex(expr)` registers (as a C-API handle) as VARCHAR but the declared
/// type-expression IS a DuckDB SQL type name, so we surface the declared expr.
fn sql_type_name(lt: &reg::LogicalType) -> String {
    match lt {
        reg::LogicalType::Boolean => "BOOLEAN".to_string(),
        reg::LogicalType::Int64 => "BIGINT".to_string(),
        reg::LogicalType::Uint64 => "UBIGINT".to_string(),
        reg::LogicalType::Float64 => "DOUBLE".to_string(),
        reg::LogicalType::Text => "VARCHAR".to_string(),
        reg::LogicalType::Blob => "BLOB".to_string(),
        reg::LogicalType::Int32 => "INTEGER".to_string(),
        reg::LogicalType::Timestamp => "TIMESTAMP".to_string(),
        reg::LogicalType::Int8 => "TINYINT".to_string(),
        reg::LogicalType::Int16 => "SMALLINT".to_string(),
        reg::LogicalType::Uint8 => "UTINYINT".to_string(),
        reg::LogicalType::Uint16 => "USMALLINT".to_string(),
        reg::LogicalType::Uint32 => "UINTEGER".to_string(),
        reg::LogicalType::Float32 => "FLOAT".to_string(),
        reg::LogicalType::Date => "DATE".to_string(),
        reg::LogicalType::Time => "TIME".to_string(),
        // reg_duckdb.rs maps Timestamptz -> LogicalTypeId::TimestampTZ.
        reg::LogicalType::Timestamptz => "TIMESTAMP WITH TIME ZONE".to_string(),
        reg::LogicalType::Interval => "INTERVAL".to_string(),
        reg::LogicalType::Uuid => "UUID".to_string(),
        // reg_duckdb.rs: T_DECIMAL -> LogicalTypeHandle::decimal(18, 3).
        reg::LogicalType::Decimal => "DECIMAL(18,3)".to_string(),
        // The declared type-expression (e.g. "INTEGER[]", "STRUCT(a INTEGER)").
        // reg_duckdb.rs registers it as VARCHAR for dispatch, but the catalog
        // should surface the user-visible declared SQL type.
        reg::LogicalType::Complex(expr) => expr.clone(),
    }
}

fn arg_json(arg: &reg::FuncArg) -> Value {
    json!({
        "name": arg.name.clone(),
        "type": sql_type_name(&arg.logical),
    })
}

/// Minimal, no-op services sink (no live DB / config). Mirrors the native
/// extension's `NativeServices`: enough to satisfy a component's `load()`, which
/// only registers functions.
struct NoopServices;

impl ExtensionServices for NoopServices {
    fn provider_version(&mut self) -> Result<String, ConfigError> {
        Ok(concat!("catalog-sig-extract/", env!("CARGO_PKG_VERSION")).to_string())
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
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Components targeting DuckDB may use wasm exceptions; mirror the host.
    config.wasm_exceptions(true);
    if let Ok(cache) = wasmtime::Cache::from_file(None) {
        config.cache(Some(cache));
    }
    Engine::new(&config)
        .map_err(anyhow::Error::from)
        .context("failed to create wasmtime engine")
}

/// Load one component and emit its `functions` JSON array (scalars, tables,
/// aggregates). Returns the array plus a (scalars, tables, aggregates) count.
fn extract_functions(engine: &Engine, name: &str, path: &Path) -> Result<(Value, (usize, usize, usize))> {
    let component = Component::from_file(engine, path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading component at {}", path.display()))?;
    let wasi: WasiCtx = WasiCtxBuilder::new().inherit_env().inherit_stdio().build();
    let callbacks = Arc::new(Mutex::new(CallbackRegistry::new()));
    // Supply an EMPTY compose:dynlink provider registry so components that import
    // `compose:dynlink/linker@0.1.0` (e.g. mlkmeans, spatialproj) still
    // instantiate — they only register their functions at load() time and do not
    // call the provider during load, so an empty registry is enough to introspect
    // their signatures. Components that don't import the linker are unaffected.
    let dynlink_registry = ProviderRegistry::new(engine.clone());
    let mut instance = load_component_with_dynlink(
        engine,
        &component,
        wasi,
        Box::new(NoopServices),
        callbacks,
        name.to_string(),
        Some(dynlink_registry),
    )
    .map_err(anyhow::Error::from)
    .context("running component load()")?;

    let pending: PendingRegistrationsData = instance.drain_pending();
    let mut funcs: Vec<Value> = Vec::new();

    for s in &pending.scalars {
        funcs.push(json!({
            "name": s.name,
            "kind": "scalar",
            "arguments": s.arguments.iter().map(arg_json).collect::<Vec<_>>(),
            "returns": sql_type_name(&s.returns),
        }));
    }
    for t in &pending.tables {
        funcs.push(json!({
            "name": t.name,
            "kind": "table",
            "arguments": t.arguments.iter().map(arg_json).collect::<Vec<_>>(),
            "columns": t.columns.iter().map(|c| json!({
                "name": c.name,
                "type": sql_type_name(&c.logical),
            })).collect::<Vec<_>>(),
        }));
    }
    for a in &pending.aggregates {
        funcs.push(json!({
            "name": a.name,
            "kind": "aggregate",
            "arguments": a.arguments.iter().map(arg_json).collect::<Vec<_>>(),
            "returns": sql_type_name(&a.returns),
        }));
    }

    let counts = (pending.scalars.len(), pending.tables.len(), pending.aggregates.len());
    Ok((Value::Array(funcs), counts))
}

struct Skip {
    name: String,
    reason: String,
}

fn main() -> Result<()> {
    let mut catalog_path = PathBuf::from("registry/index.json");
    let mut out_path: Option<PathBuf> = None;
    let mut report_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--catalog" => catalog_path = PathBuf::from(args.next().ok_or_else(|| anyhow!("--catalog needs a value"))?),
            "--out" => out_path = Some(PathBuf::from(args.next().ok_or_else(|| anyhow!("--out needs a value"))?)),
            "--report" => report_path = Some(PathBuf::from(args.next().ok_or_else(|| anyhow!("--report needs a value"))?)),
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }

    let catalog_dir = catalog_path
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let raw = std::fs::read_to_string(&catalog_path)
        .with_context(|| format!("reading catalog {}", catalog_path.display()))?;
    let catalog: Value = serde_json::from_str(&raw).context("parsing catalog JSON")?;

    let entries = catalog
        .get("extensions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("catalog has no `extensions` array"))?;
    let total = entries.len();

    let engine = build_engine()?;

    let mut enriched = 0usize;
    let mut skips: Vec<Skip> = Vec::new();
    let mut failures: Vec<Skip> = Vec::new();
    let mut kind_totals: BTreeMap<&'static str, usize> = BTreeMap::new();
    // name -> functions[]: the sidecar the Python merge step folds into the
    // catalog. Insertion-ordered (preserve_order) so it follows entry order.
    let mut sidecar = Map::new();

    for entry in entries.iter() {
        let obj = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>")
            .to_string();
        let source = obj.get("source").and_then(Value::as_str).unwrap_or("");
        let status = obj.get("status").and_then(Value::as_str).unwrap_or("");
        let artifact = obj.get("artifact").and_then(Value::as_str).map(str::to_string);

        // builtin / planned entries have no loadable artifact.
        if status == "planned" {
            skips.push(Skip { name, reason: "status=planned".to_string() });
            continue;
        }
        let artifact = match artifact {
            Some(a) if !a.is_empty() => a,
            _ => {
                skips.push(Skip { name, reason: format!("no artifact (source={source})") });
                continue;
            }
        };
        let art_path = catalog_dir.join(&artifact);
        if !art_path.exists() {
            skips.push(Skip { name, reason: format!("artifact missing locally: {artifact}") });
            continue;
        }

        match extract_functions(&engine, &name, &art_path) {
            Ok((funcs, (ns, nt, na))) => {
                *kind_totals.entry("scalar").or_default() += ns;
                *kind_totals.entry("table").or_default() += nt;
                *kind_totals.entry("aggregate").or_default() += na;
                sidecar.insert(name, funcs);
                enriched += 1;
            }
            Err(e) => {
                let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
                failures.push(Skip { name, reason: chain.join(": ") });
            }
        }
    }

    eprintln!(
        "[catalog-sig-extract] total={total} enriched={enriched} skipped={} failed={}",
        skips.len(),
        failures.len()
    );
    eprintln!(
        "[catalog-sig-extract] functions: scalar={} table={} aggregate={}",
        kind_totals.get("scalar").copied().unwrap_or(0),
        kind_totals.get("table").copied().unwrap_or(0),
        kind_totals.get("aggregate").copied().unwrap_or(0),
    );
    for s in &skips {
        eprintln!("  SKIP {}: {}", s.name, s.reason);
    }
    for f in &failures {
        eprintln!("  FAIL {}: {}", f.name, f.reason);
    }

    if let Some(op) = &out_path {
        let mut out = serde_json::to_string_pretty(&Value::Object(sidecar))?;
        out.push('\n');
        std::fs::write(op, out).with_context(|| format!("writing {}", op.display()))?;
        eprintln!("[catalog-sig-extract] wrote sidecar {}", op.display());
    } else {
        eprintln!("[catalog-sig-extract] dry-run (pass --out <file> to emit the functions sidecar)");
    }

    if let Some(rp) = report_path {
        let report = json!({
            "total": total,
            "enriched": enriched,
            "skipped": skips.iter().map(|s| json!({"name": s.name, "reason": s.reason})).collect::<Vec<_>>(),
            "failed": failures.iter().map(|f| json!({"name": f.name, "reason": f.reason})).collect::<Vec<_>>(),
            "function_counts": {
                "scalar": kind_totals.get("scalar").copied().unwrap_or(0),
                "table": kind_totals.get("table").copied().unwrap_or(0),
                "aggregate": kind_totals.get("aggregate").copied().unwrap_or(0),
            },
        });
        let mut out = serde_json::to_string_pretty(&report)?;
        out.push('\n');
        std::fs::write(&rp, out)?;
        eprintln!("[catalog-sig-extract] wrote report {}", rp.display());
    }

    Ok(())
}
