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

use std::path::{Path, PathBuf};

use anyhow::Result;
use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{AsContextMut, Engine, Store};
use wasmtime_wasi::p2::{self, pipe::MemoryInputPipe};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::driver_tool_bindings::duckdb::driver::exec as driver_exec_bindings;
use crate::driver_tool_bindings::{CronDriverTool, CronDriverToolPre};
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
        let state = open_driver_core_with_bootstrap(
            engine,
            artifacts,
            preopens,
            db_path,
            bootstrap_sql,
        )?;
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

impl wasmtime::component::HasData for DriverStoreState {
    type Data<'a> = &'a mut DriverStoreState;
}

impl driver_exec_bindings::Host for DriverStoreState {
    fn open(
        &mut self,
        path: wasmtime::component::__internal::String,
    ) -> std::result::Result<Resource<DriverConnection>, String> {
        let preopen_refs: Vec<(&Path, &str)> = self
            .preopens
            .iter()
            .map(|(h, g)| (h.as_path(), g.as_str()))
            .collect();
        let conn =
            DriverConnection::open(&self.engine, &self.artifacts, &preopen_refs, path.as_str())
                .map_err(|e| format!("driver-exec open: {e:#}"))?;
        self.table
            .push(conn)
            .map_err(|e| format!("driver-exec open: resource table full: {e}"))
    }
}

impl driver_exec_bindings::HostConnection for DriverStoreState {
    fn exec(
        &mut self,
        rep: Resource<DriverConnection>,
        sql: wasmtime::component::__internal::String,
    ) -> std::result::Result<u64, String> {
        let conn = self
            .table
            .get_mut(&rep)
            .map_err(|e| format!("driver-exec exec: bad handle: {e}"))?;
        conn.exec(sql.as_str())
    }

    fn query(
        &mut self,
        rep: Resource<DriverConnection>,
        sql: wasmtime::component::__internal::String,
    ) -> std::result::Result<
        wasmtime::component::__internal::Vec<wasmtime::component::__internal::Vec<wasmtime::component::__internal::String>>,
        String,
    > {
        let conn = self
            .table
            .get_mut(&rep)
            .map_err(|e| format!("driver-exec query: bad handle: {e}"))?;
        let rows = conn.query(sql.as_str())?;
        Ok(rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(wasmtime::component::__internal::String::from)
                    .collect::<wasmtime::component::__internal::Vec<_>>()
            })
            .collect())
    }

    fn drop(&mut self, rep: Resource<DriverConnection>) -> wasmtime::Result<()> {
        // Drop the underlying DriverConnection; ignore-not-found is fine —
        // wasmtime already guarantees at-most-once drop.
        let _ = self.table.delete(rep);
        Ok(())
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

    let mut linker = Linker::<DriverStoreState>::new(&engine);
    p2::add_to_linker_sync(&mut linker)?;
    driver_exec_bindings::add_to_linker::<DriverStoreState, DriverStoreState>(
        &mut linker,
        |s| s,
    )?;

    let component = Component::from_file(&engine, tool_wasm).map_err(|e| {
        anyhow::anyhow!(
            "failed to load cron-driver-tool component from {}: {e}",
            tool_wasm.display()
        )
    })?;
    let instance_pre = linker.instantiate_pre(&component)?;
    let tool_pre = CronDriverToolPre::new(instance_pre)?;
    let tool: CronDriverTool = tool_pre.instantiate(store.as_context_mut())?;
    let result = tool.wasi_cli_run().call_run(store.as_context_mut())?;
    Ok(result)
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
            .preopened_dir(host, guest, DirPerms::all(), FilePerms::all())
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
