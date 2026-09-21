//! Host implementation of `duckdb:driver/exec@5.0.0` — the WIT surface the
//! cron-driver-tool (and future wasi:cli/run scheduler / poller tools)
//! import for running SQL against a real DuckDB core.
//!
//! ## Shape
//!
//! The tool is a wasi:cli/run component. It imports:
//!   * `duckdb:driver/exec.{open, connection.{exec, query}}`
//!   * `wasi:clocks/monotonic-clock`, `wasi:clocks/wall-clock`, `wasi:io/poll`
//!   * `wasi:cli/{environment, stderr, stdout, run}`
//!
//! `run_driver_tool()` instantiates the tool through the wasmos-native
//! path (`SyncRuntime::compile_component` + `SyncRuntime::instantiate`),
//! registers the driver-exec host imports via
//! `HostImports::register_sync`, and calls the tool's
//! `wasi:cli/run.run()` through `SyncInstance::call_wasi_command`. The
//! tool enters its own tick loop and blocks on
//! `wasi:clocks/monotonic-clock.subscribe-duration(...)` between ticks.
//! Exiting is: the tool returns from `run()` (only happens in `--once`
//! mode) or the host is signalled (SIGINT propagates through the
//! private tokio runtime SyncRuntime owns).
//!
//! ## Dispatch model
//!
//! Each `duckdb:driver/exec.open(path)` call brings up a **persistent**
//! wasm core (one `wasmtime::Store` + `CoreExecution` + `ExtensionManager`)
//! and opens a real DuckDB connection against `path`. That state is stored
//! in the wasmtime `ResourceTable` and survives across every
//! `connection.exec` / `connection.query` invocation for the resource's
//! lifetime. The two cron extensions are LOADed once at open; there is no
//! per-call bootstrap and no per-call wasm instantiation cost — a tick that
//! fires N jobs pays exactly N + 2 core `execute` calls (read due + one
//! advance + one per job) against the same connection.
//!
//! This replaces the earlier MVP that spawned a fresh `run_cli_capture`
//! per SQL call, prepended `LOAD cron; LOAD cron_scheduler;` to every
//! script, and CSV-scraped the CLI's box-mode output.
//!
//! ## Migration note (ADR-0029, see `docs/wasmos-migration-recipe.md`)
//!
//! - **Phase 2b (Path A)** — the former
//!   `wasmtime::component::bindgen!` sites for
//!   `duckdb:driver/exec@5.0.0` were replaced by a single
//!   `SyncHostCall` handler dispatching by kebab-cased method name
//!   (`"open"`, `"[method]connection.exec"`,
//!   `"[method]connection.query"`, drop routed to
//!   `on_resource_drop`). Host imports flowed through
//!   `wasmos_runtime_wasmtime_v48::sync_bridge_resource::install_host_call`
//!   (the escape-hatch bridge).
//! - **Path B (this file, 2026-09-21)** — the escape-hatch bridge is
//!   gone. `run_driver_tool` now compiles + instantiates through
//!   `wasmos_runtime_wasmtime_v48::SyncRuntime` (the sync facade
//!   over the wasmos-native async path). Host imports register via
//!   `HostImports::register_sync`. Guest exports dispatch through
//!   `SyncInstance::call_wasi_command`. The `wasmtime::Store` /
//!   `wasmtime::Engine` / `wasmtime::component::{Component, Linker,
//!   Instance}` names no longer appear in this file except for
//!   `DriverStoreState.engine` (which
//!   `DriverConnection::open` still needs to bring up the persistent
//!   DuckDB core wasm — legitimate ducklink-host-internal use, not a
//!   consumer-side leak).
//!
//! The wasm-side `connection` resource type is auto-registered by
//! the wasmos-native adapter's `wire_host_imports` — same
//! `ResourceType::host_dynamic(N)` mechanism, just moved from the
//! escape-hatch bridge to the wasmos-native path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use wasmos_runtime_api::{
    CompileOptions, ComponentSource, ExecutionContext, HostCallContext, HostImports, Preopen,
    Resource as WasmosResource, ResourceTable as WasmosResourceTable, RuntimeConfig, RuntimeError,
    RuntimeResult, SyncHostCall, Value, WasiEnvironment,
};
use wasmos_runtime_wasmtime_v48::SyncRuntime;
use wasmtime::Engine;

use crate::{
    build_engine_for_driver, driver_core_exec, driver_core_query, open_driver_core,
    open_driver_core_with_bootstrap, ComponentArtifacts, DriverCoreState,
};

/// One connection resource, as the wasm tool sees it. Owns the wasm core
/// alive across every SQL call the tool makes: `open()` instantiates the
/// core + opens a real DuckDB connection + LOADs the two cron extensions
/// once; `exec/query` reuse that same store; dropping the resource drops
/// the core (via `ResourceTable::delete`).
pub struct DriverConnection {
    state: DriverCoreState,
}

impl DriverConnection {
    pub fn open(
        engine: &Engine,
        artifacts: &ComponentArtifacts,
        preopens: &[(&Path, &str)],
        path: &str,
    ) -> Result<Self> {
        // Empty string per the WIT contract means `:memory:`; the facade
        // takes `Option<&str>` — a `None` there is the same instruction.
        let db_path = if path.is_empty() { None } else { Some(path) };
        let state = open_driver_core(engine, artifacts, preopens, db_path)?;
        Ok(Self { state })
    }

    /// Same as [`open`] but with caller-supplied bootstrap SQL. Each
    /// string in `bootstrap_sql` is run against the fresh connection
    /// before the wrapper returns; pass `&[]` to skip bootstrap entirely
    /// (a bare core, no extensions loaded). Intended entry point for
    /// out-of-tree embedders that don't want cron loaded — e.g. an
    /// RxNorm ingest pipeline that runs only `CREATE TABLE` + `COPY FROM`
    /// + `CREATE INDEX` and does not need any wasm extensions.
    ///
    /// [`open`]: DriverConnection::open
    pub fn open_with_bootstrap(
        engine: &Engine,
        artifacts: &ComponentArtifacts,
        preopens: &[(&Path, &str)],
        path: &str,
        bootstrap_sql: &[&str],
    ) -> Result<Self> {
        let db_path = if path.is_empty() { None } else { Some(path) };
        let state =
            open_driver_core_with_bootstrap(engine, artifacts, preopens, db_path, bootstrap_sql)?;
        Ok(Self { state })
    }

    pub fn exec(&mut self, sql: &str) -> std::result::Result<u64, String> {
        driver_core_exec(&mut self.state, sql)
    }

    pub fn query(&mut self, sql: &str) -> std::result::Result<Vec<Vec<String>>, String> {
        driver_core_query(&mut self.state, sql)
    }
}

/// Ducklink-owned driver-tool state, plumbed through
/// [`ExecutionContext::with_consumer_state`] on the wasmos-native
/// path. Host handlers reach it from a [`HostCallContext`] via
/// `ctx.consumer_state::<DriverStoreState>()`.
///
/// The wasi context + wasi resource table + wasi-http context live
/// inside the adapter's `AdapterHostState` (internal to
/// wasmos-runtime-wasmtime-v48), so this struct carries only its
/// domain fields: the wasmos `ResourceTable` for `DriverConnection`
/// tracking + the (engine, artifacts, preopens) triple each new
/// `DriverConnection` needs to bring up its own persistent core.
///
/// The `engine` field is a `wasmtime::Engine` because
/// `DriverConnection::open` (called from the host handler) spins up
/// the persistent DuckDB core wasm inside its own wasmtime store —
/// legitimate ducklink-host-internal use, not a consumer-side
/// wasmtime leak. Full migration of that machinery to wasmos-native
/// is a separate multi-file rewrite (see the migration recipe).
struct DriverStoreState {
    /// `wasmos_runtime_api::ResourceTable` for our own
    /// `DriverConnection` tracking. Split-tables step retired the
    /// direct dep on `wasmtime::component::Resource<T>` in consumer
    /// code.
    conn_table: WasmosResourceTable,
    engine: Engine,
    artifacts: ComponentArtifacts,
    /// Preopens the tool inherits so `open("some/rel.duckdb")` resolves
    /// against the same cwd as the enclosing `ducklink cron` process.
    preopens: Vec<(PathBuf, String)>,
}

/// Interface name shared between host-import registration and every
/// `ctx.new_host_resource` mint site. Matches the WIT declaration
/// verbatim (`package duckdb:driver@5.0.0; interface exec` in
/// `extensions/cron-driver-tool/wit/deps/duckdb-driver/exec.wit`).
/// Wasmos does verbatim interface-name matching including version
/// tags; a mismatch surfaces as `MissingImport` at instantiate time.
const EXEC_IFACE: &str = "duckdb:driver/exec@5.0.0";

/// Wasm-side resource name for the connection handle. Same string is
/// used in both the drop callback's `resource_name` check and every
/// `new_host_resource` mint site.
const CONN_RESOURCE: &str = "connection";

/// Wasmos-native host implementation of `duckdb:driver/exec@5.0.0`.
/// Stateless — every call reaches store state via
/// `ctx.consumer_state::<DriverStoreState>()`, matching the bindgen-era
/// pattern where the same store data was reached through the
/// bindgen-generated `Host` accessor.
struct DriverExecHost;

impl SyncHostCall for DriverExecHost {
    fn call(
        &self,
        ctx: &mut HostCallContext<'_>,
        method: &str,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        match method {
            "open" => self.host_open(ctx, args),
            "[method]connection.exec" => self.host_exec(ctx, args),
            "[method]connection.query" => self.host_query(ctx, args),
            other => Err(RuntimeError::msg(format!(
                "{EXEC_IFACE}: unknown method {other:?}"
            ))),
        }
    }

    fn on_resource_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        resource_name: &str,
        rep: u32,
    ) -> RuntimeResult<()> {
        if resource_name != CONN_RESOURCE {
            return Err(RuntimeError::msg(format!(
                "{EXEC_IFACE}: unexpected resource drop for {resource_name:?}"
            )));
        }
        let state = ctx.consumer_state::<DriverStoreState>().ok_or_else(|| {
            RuntimeError::msg("driver-exec drop: consumer_state<DriverStoreState> unavailable")
        })?;
        // Ignore-not-found matches the bindgen-era `let _ =
        // self.table.delete(rep);` — wasmtime guarantees at-most-once
        // drop, but the bridge routes here even if the entry was
        // already reaped through another path (e.g. a store teardown
        // in-flight).
        let _ = state
            .conn_table
            .delete(WasmosResource::<DriverConnection>::from_raw(rep, true));
        Ok(())
    }
}

impl DriverExecHost {
    /// `duckdb:driver/exec.open(path: string) -> result<connection, string>`
    fn host_open(
        &self,
        ctx: &mut HostCallContext<'_>,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        let path = match args.as_slice() {
            [Value::String(p)] => p.clone(),
            other => {
                return Err(RuntimeError::msg(format!(
                    "{EXEC_IFACE}.open: expected [Value::String], got {other:?}"
                )))
            }
        };
        let state = ctx.consumer_state::<DriverStoreState>().ok_or_else(|| {
            RuntimeError::msg("driver-exec open: consumer_state<DriverStoreState> unavailable")
        })?;
        // Snapshot preopens through borrowed refs — mirrors the
        // bindgen-era impl at line 139-143 verbatim.
        let preopen_refs: Vec<(&Path, &str)> = state
            .preopens
            .iter()
            .map(|(h, g)| (h.as_path(), g.as_str()))
            .collect();
        match DriverConnection::open(&state.engine, &state.artifacts, &preopen_refs, &path) {
            Ok(conn) => {
                let handle = state.conn_table.push(conn).map_err(|e| {
                    RuntimeError::msg(format!("driver-exec open: resource table full: {e}"))
                })?;
                let rep = handle.handle();
                let resource_value = ctx.new_host_resource(EXEC_IFACE, CONN_RESOURCE, rep)?;
                Ok(vec![Value::Result(Ok(Some(Box::new(resource_value))))])
            }
            Err(e) => Ok(vec![Value::Result(Err(Some(Box::new(Value::String(
                format!("driver-exec open: {e:#}"),
            )))))]),
        }
    }

    /// `duckdb:driver/exec.connection.exec(sql: string) -> result<u64, string>`
    fn host_exec(
        &self,
        ctx: &mut HostCallContext<'_>,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        let (rep_value, sql) = match args.as_slice() {
            [r @ Value::Resource { .. }, Value::String(s)] => (r.clone(), s.clone()),
            other => {
                return Err(RuntimeError::msg(format!(
                    "{EXEC_IFACE}.[method]connection.exec: expected \
                     [Value::Resource, Value::String], got {other:?}"
                )))
            }
        };
        let rep = ctx.resource_rep(&rep_value)?;
        let state = ctx.consumer_state::<DriverStoreState>().ok_or_else(|| {
            RuntimeError::msg("driver-exec exec: consumer_state<DriverStoreState> unavailable")
        })?;
        // The rep is stable across the bridge round-trip; the same
        // rep the guest sees is the same one the wasmos ResourceTable
        // indexed at push time.
        let handle = WasmosResource::<DriverConnection>::from_raw(rep, true);
        let conn = state
            .conn_table
            .get_mut(&handle)
            .map_err(|e| RuntimeError::msg(format!("driver-exec exec: bad handle: {e}")))?;
        match conn.exec(&sql) {
            Ok(n) => Ok(vec![Value::Result(Ok(Some(Box::new(Value::U64(n)))))]),
            Err(e) => Ok(vec![Value::Result(Err(Some(Box::new(Value::String(e)))))]),
        }
    }

    /// `duckdb:driver/exec.connection.query(sql: string) ->
    /// result<list<list<string>>, string>`
    fn host_query(
        &self,
        ctx: &mut HostCallContext<'_>,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        let (rep_value, sql) = match args.as_slice() {
            [r @ Value::Resource { .. }, Value::String(s)] => (r.clone(), s.clone()),
            other => {
                return Err(RuntimeError::msg(format!(
                    "{EXEC_IFACE}.[method]connection.query: expected \
                     [Value::Resource, Value::String], got {other:?}"
                )))
            }
        };
        let rep = ctx.resource_rep(&rep_value)?;
        let state = ctx.consumer_state::<DriverStoreState>().ok_or_else(|| {
            RuntimeError::msg("driver-exec query: consumer_state<DriverStoreState> unavailable")
        })?;
        let handle = WasmosResource::<DriverConnection>::from_raw(rep, true);
        let conn = state
            .conn_table
            .get_mut(&handle)
            .map_err(|e| RuntimeError::msg(format!("driver-exec query: bad handle: {e}")))?;
        match conn.query(&sql) {
            Ok(rows) => {
                // Encode list<list<string>> as
                // Value::List(Vec<Value::List(Vec<Value::String>)>).
                let outer: Vec<Value> = rows
                    .into_iter()
                    .map(|row| {
                        let inner: Vec<Value> = row.into_iter().map(Value::String).collect();
                        Value::List(inner)
                    })
                    .collect();
                Ok(vec![Value::Result(Ok(Some(Box::new(Value::List(outer)))))])
            }
            Err(e) => Ok(vec![Value::Result(Err(Some(Box::new(Value::String(e)))))]),
        }
    }
}

/// Instantiate the cron-driver tool component and drive it to completion.
///
/// * `tool_wasm` — path to the built `cron_driver_tool.wasm`.
/// * `db` — DB path the tool should open (materialized into argv[1]).
/// * `artifacts` — the composed core + CLI wasm each persistent
///   `DriverConnection` spawns its own core from.
/// * `preopens` — host->guest preopen tuples (e.g. `(cwd, ".")`) inherited
///   by the tool AND passed through to each persistent core.
/// * `extra_args` — `--interval-secs N` / `--once` after the DB positional.
///
/// Returns `Ok(Ok(()))` when the tool's `run()` returned normally, or
/// `Ok(Err(()))` when it returned an error (the tool's stderr already
/// carries the diagnostic). Trap-shaped errors bubble up as `Err`.
pub fn run_driver_tool(
    tool_wasm: &Path,
    db: &Path,
    artifacts: &ComponentArtifacts,
    preopens: &[(&Path, &str)],
    extra_args: &[String],
) -> Result<Result<(), ()>> {
    // DriverStoreState holds a wasmtime::Engine because
    // DriverConnection::open (called from the DriverExecHost handler)
    // spins up a persistent DuckDB core wasm inside its own store —
    // wasmtime-native ducklink-host machinery, not a consumer-side
    // wasmtime leak. The Engine we hand it is the same one used by
    // SyncRuntime under the hood (both come from
    // `build_engine_for_driver`), so per-core startup is warm-cache.
    let engine = build_engine_for_driver()?;

    // Duplicate the preopens so we can hand one copy to the tool's WasiCtx
    // and store the other on `DriverStoreState` (for persistent cores to
    // re-preopen against the same shape).
    let owned_preopens: Vec<(PathBuf, String)> = preopens
        .iter()
        .map(|(host, guest)| (host.to_path_buf(), (*guest).to_string()))
        .collect();

    // argv shape: [argv0="cron-driver-tool", <db>, extras...]. The tool
    // does `.iter().skip(1)` so argv[0] is dropped; the DB positional and
    // `--interval-secs N` / `--once` land in `parse_args()` unchanged.
    let mut argv: Vec<String> = Vec::with_capacity(2 + extra_args.len());
    argv.push("cron-driver-tool".to_string());
    argv.push(db.display().to_string());
    argv.extend_from_slice(extra_args);

    let wasi_env = build_driver_wasi_env(&argv, preopens);

    // Wasmos-native path — SyncRuntime owns the wasmtime Store / Engine /
    // Linker / Component / Instance internally. This function's public
    // surface names no wasmtime types beyond the DriverStoreState.engine
    // field that DriverConnection::open() still requires.
    let sync_rt = SyncRuntime::new(RuntimeConfig::default())
        .map_err(|e| anyhow::anyhow!("build SyncRuntime: {e}"))?;

    let compiled = sync_rt
        .compile_component(
            ComponentSource::Path {
                path: tool_wasm.to_path_buf(),
                name: Some("cron-driver-tool".to_string()),
            },
            CompileOptions::default(),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to compile cron-driver-tool component from {}: {e}",
                tool_wasm.display()
            )
        })?;

    let driver_state = DriverStoreState {
        conn_table: WasmosResourceTable::new(),
        engine,
        artifacts: artifacts.clone(),
        preopens: owned_preopens,
    };

    // Register the duckdb:driver/exec host imports via HostImports.
    // The adapter's wire_host_imports auto-registers the `connection`
    // resource type by introspecting the component's imports at
    // instantiate time — same shape sync_bridge_resource used to do.
    let host_imports = HostImports::new().register_sync(EXEC_IFACE, DriverExecHost);

    let ctx = ExecutionContext::new()
        .with_wasi(wasi_env)
        .with_host_imports(host_imports)
        .with_consumer_state(driver_state);

    let mut instance = sync_rt
        .instantiate(&compiled, ctx)
        .map_err(|e| anyhow::anyhow!("instantiate cron-driver-tool: {e}"))?;

    // Wasmos-native wasi:cli/run.run dispatcher — mirrors the tool's
    // `wasi_cli_run().call_run(...)` bindgen path with the two-arm
    // unpacking already handled inside SyncInstance::call_wasi_command.
    instance
        .call_wasi_command()
        .map_err(|e| anyhow::anyhow!("driver-tool wasi:cli/run.run(): {e}"))
}

/// Build the portable WASI environment description for the driver
/// tool: inherits the parent process's stdio/env/network so the
/// tool's stderr log lines appear alongside the caller's, and grants
/// the same preopens the persistent cores will see.
///
/// The tool does no stdin reads, so `inherit_stdin: false` (closed
/// stdin) is equivalent to the pre-SyncStoreState pattern of feeding
/// an empty `MemoryInputPipe` — both return EOF on read.
fn build_driver_wasi_env(args: &[String], preopens: &[(&Path, &str)]) -> WasiEnvironment {
    let mut env = WasiEnvironment::sandboxed()
        .with_args(args.iter().cloned())
        .inherit_env()
        .inherit_stdout()
        .inherit_stderr()
        .with_network();
    for (host, guest) in preopens {
        env = env.with_preopen(Preopen::read_write(host.to_path_buf(), (*guest).to_string()));
    }
    env
}

/// Locate the cron-driver-tool wasm alongside the extension artifacts.
/// The Makefile copies release builds to `artifacts/dotcmds/` for tools;
/// for cron-driver-tool we colocate with extensions to keep one root.
pub fn default_tool_path() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    // Preferred: an ext-shipped copy. Fall back to the target/ path so
    // `cargo component build` output "just works" pre-install.
    for candidate in [
        cwd.join("artifacts/extensions/cron_driver_tool.wasm"),
        cwd.join("target/wasm32-wasip2/release/cron_driver_tool.wasm"),
        cwd.join("target/wasm32-wasip1/release/cron_driver_tool.wasm"),
    ] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "cron-driver-tool wasm not found. Build with: \
         cargo component build -p cron-driver-tool --target wasm32-wasip2 --release"
    )
}
