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
//! `run_driver_tool()` instantiates the tool with a fresh wasmtime store,
//! wires the driver-exec Linker binding to this module, and calls the
//! tool's `wasi:cli/run.run()` — which enters its own tick loop and blocks
//! on `wasi:clocks/monotonic-clock.subscribe-duration(...)` between ticks.
//! Exiting is: the tool returns from `run()` (only happens in `--once`
//! mode) or the host is signalled (SIGINT propagates through wasmtime).
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
//! ## Migration note (Phase 2b of ADR-0029, see
//! `docs/wasmos-migration-recipe.md`)
//!
//! The former `wasmtime::component::bindgen!` sites for
//! `duckdb:driver/exec@5.0.0` (`impl Host` + `impl HostConnection`
//! for `DriverStoreState`) are gone. Host imports now flow through
//! `wasmos_runtime_wasmtime_v48::sync_bridge_resource::install_host_call`
//! with a single `SyncHostCall` handler that dispatches by kebab-cased
//! method name (`"open"`, `"[method]connection.exec"`,
//! `"[method]connection.query"`, drop routed to `on_resource_drop`).
//! The bindgen `with:` map that bound the WIT `connection` resource to
//! this crate's native `DriverConnection` becomes explicit: the bridge
//! auto-registers the wasm-side resource type via
//! `ResourceType::host_dynamic(N)`, and this handler stores
//! `DriverConnection` in the wasmtime `ResourceTable` under the rep the
//! bridge assigns — same storage shape, no lookup magic. Guest export
//! `wasi:cli/run@0.2.6.run` dispatches through `sync_export_bridge`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use wasmos_runtime_api::{
    HostCallContext, RuntimeError, RuntimeResult, SyncHostCall, Value,
};
use wasmos_runtime_wasmtime_v48::{sync_bridge_resource, sync_export_bridge};
use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{AsContextMut, Engine, Store};
use wasmtime_wasi::p2::{self, pipe::MemoryInputPipe};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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

/// Store state for the driver-tool wasmtime run. Carries the WASI ctx +
/// resource table plus the (engine, artifacts, preopens) triple each new
/// `DriverConnection` needs to bring up its own persistent core.
///
/// The engine is shared with the tool's own store — same compile cache,
/// same wasm feature flags — so per-connection core startup is warm-cache
/// after the first run of a given ducklink binary.
struct DriverStoreState {
    wasi: WasiCtx,
    table: ResourceTable,
    engine: Engine,
    artifacts: ComponentArtifacts,
    /// Preopens the tool inherits so `open("some/rel.duckdb")` resolves
    /// against the same cwd as the enclosing `ducklink cron` process.
    preopens: Vec<(PathBuf, String)>,
}

impl WasiView for DriverStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
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
        let state = ctx
            .consumer_state::<DriverStoreState>()
            .ok_or_else(|| {
                RuntimeError::msg("driver-exec drop: consumer_state<DriverStoreState> unavailable")
            })?;
        // Ignore-not-found matches the bindgen-era `let _ =
        // self.table.delete(rep);` — wasmtime guarantees at-most-once
        // drop, but the bridge routes here even if the entry was
        // already reaped through another path (e.g. a store teardown
        // in-flight).
        let _ = state.table.delete(Resource::<DriverConnection>::new_own(rep));
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
        let state = ctx
            .consumer_state::<DriverStoreState>()
            .ok_or_else(|| {
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
                let handle = state.table.push(conn).map_err(|e| {
                    RuntimeError::msg(format!(
                        "driver-exec open: resource table full: {e}"
                    ))
                })?;
                let rep = handle.rep();
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
        let state = ctx
            .consumer_state::<DriverStoreState>()
            .ok_or_else(|| {
                RuntimeError::msg("driver-exec exec: consumer_state<DriverStoreState> unavailable")
            })?;
        // `Resource::new_own(rep)` — the rep is stable across the
        // bridge round-trip; the same rep the guest sees is the same
        // one the wasmtime ResourceTable indexed at push time.
        let handle = Resource::<DriverConnection>::new_own(rep);
        let conn = state
            .table
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
        let state = ctx
            .consumer_state::<DriverStoreState>()
            .ok_or_else(|| {
                RuntimeError::msg("driver-exec query: consumer_state<DriverStoreState> unavailable")
            })?;
        let handle = Resource::<DriverConnection>::new_own(rep);
        let conn = state
            .table
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

    let wasi = build_driver_wasi(&argv, preopens)?;

    let state = DriverStoreState {
        wasi,
        table: ResourceTable::new(),
        engine: engine.clone(),
        artifacts: artifacts.clone(),
        preopens: owned_preopens,
    };
    let mut store = Store::new(&engine, state);

    // Load the component BEFORE wiring the exec host imports — the
    // bridge introspects the component's imported interfaces to
    // determine which resource types to auto-register. Bindgen's
    // `add_to_linker` did not need this because the generated code
    // knew the resource shape at macro-expansion time.
    let component = Component::from_file(&engine, tool_wasm).map_err(|e| {
        anyhow::anyhow!(
            "failed to load cron-driver-tool component from {}: {e}",
            tool_wasm.display()
        )
    })?;

    let mut linker = Linker::<DriverStoreState>::new(&engine);
    p2::add_to_linker_sync(&mut linker)?;
    // The bridge replaces
    //   driver_exec_bindings::add_to_linker::<DriverStoreState, DriverStoreState>(...)
    // — one call registers the `connection` resource + all three
    // methods (`open`, `[method]connection.exec`,
    // `[method]connection.query`) via the DriverExecHost SyncHostCall
    // impl. The bridge is a no-op if the component doesn't actually
    // import `duckdb:driver/exec@5.0.0`.
    sync_bridge_resource::install_host_call::<DriverStoreState>(
        &engine,
        &mut linker,
        &component,
        EXEC_IFACE,
        Arc::new(DriverExecHost),
    )
    .map_err(|e| anyhow::anyhow!("wire duckdb:driver/exec host: {e}"))?;

    let instance_pre = linker.instantiate_pre(&component)?;
    let instance = instance_pre.instantiate(store.as_context_mut())?;
    // The bindgen path was:
    //   let tool_pre = CronDriverToolPre::new(instance_pre)?;
    //   let tool: CronDriverTool = tool_pre.instantiate(store.as_context_mut())?;
    //   tool.wasi_cli_run().call_run(store.as_context_mut())?
    // Under the bridge, dispatch through sync_export_bridge — the
    // interface name matches the world's `export wasi:cli/run@0.2.6;`
    // verbatim; the method `run` takes no args and returns `result`
    // (both arms empty).
    let ret = sync_export_bridge::call_export(
        store.as_context_mut(),
        &instance,
        Some("wasi:cli/run@0.2.6"),
        "run",
        &[],
    )
    .map_err(|e| anyhow::anyhow!("driver-tool wasi:cli/run.run(): {e}"))?;

    // Unpack `result` — both arms carry no payload, so both
    // `Ok(None)` and `Err(None)` are the expected shapes.
    // `Ok(Some(_))` / `Err(Some(_))` are contract violations for a
    // `result` without payloads.
    match ret.as_slice() {
        [Value::Result(inner)] => match inner {
            Ok(None) => Ok(Ok(())),
            Err(None) => Ok(Err(())),
            Ok(Some(payload)) => Err(anyhow::anyhow!(
                "driver-tool run(): unexpected Ok payload {payload:?} for result<_, _>"
            )),
            Err(Some(payload)) => Err(anyhow::anyhow!(
                "driver-tool run(): unexpected Err payload {payload:?} for result<_, _>"
            )),
        },
        other => Err(anyhow::anyhow!(
            "driver-tool run(): expected exactly one Value::Result return, got {other:?}"
        )),
    }
}

/// Build a WASI context for the driver tool: inherits the parent process's
/// stdio/env/network so the tool's stderr log lines appear alongside the
/// caller's, and grants the same preopens the persistent cores will see.
fn build_driver_wasi(args: &[String], preopens: &[(&Path, &str)]) -> Result<WasiCtx> {
    let mut builder = WasiCtxBuilder::new();
    builder.args(args);
    builder.inherit_env();
    // The tool does no stdin reads; feed it an empty pipe so the WasiCtx
    // doesn't attach a real (potentially TTY) stdin.
    builder.stdin(MemoryInputPipe::new(""));
    builder.inherit_stdout();
    builder.inherit_stderr();
    // Grant outbound network / DNS on the off-chance a future driver
    // variant needs it; harmless to include today.
    builder.inherit_network();
    builder.allow_ip_name_lookup(true);
    for (host, guest) in preopens {
        builder
            .preopened_dir(host, guest, FsPerms::ReadWrite)
            .map_err(|e| {
                e.context(format!(
                    "failed to preopen directory {} as {} for cron-driver-tool",
                    host.display(),
                    guest
                ))
            })?;
    }
    Ok(builder.build())
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
