//! `ducklink cron <subcommand>` — cron-style SQL scheduler driver.
//!
//! Subcommands:
//!   * `init`                    Install __cron_jobs / __cron_runs + macros
//!   * `schedule <name> <expr> <sql>`  Upsert a job (idempotent by name)
//!   * `unschedule <name-or-id>` Remove a job
//!   * `activate <name-or-id>`   Re-arm a paused job (updates next_run_at)
//!   * `deactivate <name-or-id>` Pause a job without deleting it
//!   * `list`                    Show scheduled jobs
//!   * `run`                     Tick loop: fire due jobs, repeat every --interval
//!
//! Every subcommand takes a `--db PATH` (required — a cron over `:memory:` is
//! pointless), plus optional `--store catalog|sidecar` and
//! `--sidecar-path PATH`. In `sidecar` mode the tables live in an ATTACHed DB
//! (so the cron state is portable across DuckLink databases and does not
//! clutter the primary catalog).
//!
//! ### Driver model
//!
//! Cron ticks are pure SQL over `__cron_jobs`. The wasm side has no timers,
//! so *some* driver has to call `run` — this CLI is one; any host embedder
//! (browser tab, Cloudflare Worker, …) can run the same SQL on its own timer.
//! On each tick we:
//!
//! 1. Read due jobs via `SELECT id, sql FROM cron_due(now())`.
//! 2. Advance `next_run_at` on all due rows BEFORE firing (so a mid-tick crash
//!    doesn't cause the same window to re-fire indefinitely).
//! 3. Execute each due job's `sql` through the same persistent core connection
//!    (the facade `driver_core_exec`) and append a `__cron_runs` row with the
//!    outcome. Because `driver_core_exec` returns `Result<u64, String>` where
//!    `Err` carries the real DuckDB error text, `last_status` distinguishes
//!    `fired` from `failed` and `last_error` is genuinely populated on failure
//!    — this is the upgrade from the prior CLI-capture path that swallowed
//!    stderr and marked every fired job `fired` unconditionally.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use wasmtime::Engine;

use crate::{
    build_engine_for_driver, driver_core_exec, driver_core_query, open_driver_core,
    set_extension_root, ComponentArtifacts, DriverCoreState,
};

const HELP: &str = "\
ducklink cron — cron-style SQL scheduler

USAGE:
    ducklink cron <SUBCOMMAND>

SUBCOMMANDS:
    init                                 Install __cron_jobs / __cron_runs + macros
    schedule <NAME> <EXPR> <SQL>         Upsert a job (idempotent by NAME)
    unschedule <NAME-OR-ID>              Remove a job
    activate <NAME-OR-ID>                Re-arm a paused job (recomputes next_run_at)
    deactivate <NAME-OR-ID>              Pause a job without deleting it
    list                                 Show scheduled jobs (JSON)
    metrics                              Prometheus text-format counters/gauges
                                         over __cron_runs / __cron_jobs / leases
    run                                  Tick loop: fire due jobs, sleep, repeat

COMMON OPTIONS:
    --db <PATH>              (required) Persistent DuckDB file to schedule against
    --store catalog|sidecar  Where the cron tables live. Default: catalog.
    --sidecar-path <PATH>    Sidecar DB path. Default: <db-dir>/cron.duckdb.

schedule OPTIONS:
    --catch-up skip|run_once  Missed-window policy. Default: skip (pg_cron parity).
    --node <NAME>             Only fire this job on a driver whose identity ==
                              NAME. Omit for the default \"any driver\" (nodename
                              IS NULL in the __cron_jobs table).
    --database <NAME>         Cross-catalog target (pg_cron parity:
                              schedule_in_database). The driver USEs this
                              catalog for the job's SQL, then restores the
                              previous catalog. NAME must match a catalog
                              attached at driver startup with `run --attach
                              PATH=NAME`. Omit to run against the driver's
                              current catalog (backwards-compatible default).
    --readonly                Wrap this job's fire in BEGIN; ... ROLLBACK;
                              so any writes are undone at end. Best-effort
                              (an explicit COMMIT in the SQL still commits);
                              safety-net for jobs the operator expects to
                              be read-only.

run OPTIONS:
    --interval <SECS>        Sleep between ticks. Default: 30.
    --once                   Run one tick and exit (useful in host-driven mode).
    --history-limit <N>      Keep at most N __cron_runs rows per job (oldest
                             pruned at end of each tick). 0 or unset = no
                             pruning (v0 behaviour; the caller manages it).
    --wasm-driver            Dispatch to cron-driver-tool.wasm (wasi:cli/run)
                             instead of the native Rust loop. Portable to any
                             wasi-preview2 host. Same behaviour; slightly
                             better per-job error capture. Requires the
                             cron-driver-tool component to be built.
    --node <NAME>             This driver's identity for per-job lease
                              ownership + per-node targeting. Precedence:
                              --node flag > $DUCKLINK_CRON_NODE >
                              \"<hostname>-<pid>\" fallback. Each due job is
                              acquired independently via __cron_leases;
                              contending drivers just skip THAT job (not the
                              whole tick). Node-targeted jobs (scheduled with
                              --node NAME) fire only when this identity
                              matches; anyone-goes jobs (default) fire on any.
    --attach <PATH=NAME>      (repeatable) At startup, run
                              `ATTACH IF NOT EXISTS 'PATH' AS NAME` so
                              cross-catalog jobs (scheduled with
                              `--database NAME`) can find their target
                              catalog. Missing catalogs cause the affected
                              jobs to be marked `last_status='skipped'`
                              rather than crashing the tick.

EXAMPLES:
    ducklink cron init --db app.duckdb
    ducklink cron schedule --db app.duckdb 'nightly-vacuum' '0 3 * * *' 'CHECKPOINT'
    ducklink cron list --db app.duckdb
    ducklink cron run --db app.duckdb --interval 60
";

#[derive(Clone, Copy)]
enum Store {
    Catalog,
    Sidecar,
}

struct Common {
    db: PathBuf,
    store: Store,
    sidecar_path: Option<PathBuf>,
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Quote a DuckDB identifier (catalog / schema / table name) with double
/// quotes, escaping any embedded double quote by doubling it. Used for
/// `USE <catalog>` in the cross-catalog job wrap so catalog names with
/// hyphens, spaces, or other identifier-hostile characters still work.
fn sql_escape_identifier(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Resolve the driver's node identity for leader-election + per-node
/// targeting. Precedence: `--node` flag > `$DUCKLINK_CRON_NODE` env >
/// `hostname()-pid` fallback. Never fails: on hostname lookup failure the
/// fallback degrades to `driver-<pid>`. Returned string is safe to SQL-quote
/// via `sql_escape` at the call site.
fn resolve_node_name(cli_flag: Option<&str>) -> String {
    if let Some(v) = cli_flag {
        return v.to_string();
    }
    if let Ok(v) = std::env::var("DUCKLINK_CRON_NODE") {
        if !v.is_empty() {
            return v;
        }
    }
    let pid = std::process::id();
    let host = hostname_or_fallback();
    if host.is_empty() {
        format!("driver-{pid}")
    } else {
        format!("{host}-{pid}")
    }
}

/// Hostname lookup without a new dep. Reads `$HOSTNAME` (set on most login
/// shells; systemd exposes it), else `/etc/hostname` (Linux). Returns empty
/// string when neither is available — the caller falls back to `driver-<pid>`.
fn hostname_or_fallback() -> String {
    if let Ok(v) = std::env::var("HOSTNAME") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(v) = std::fs::read_to_string("/etc/hostname") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

fn parse_common(rest: &mut Vec<String>) -> Result<Common> {
    let mut db: Option<PathBuf> = None;
    let mut store = Store::Catalog;
    let mut sidecar_path: Option<PathBuf> = None;
    let mut kept: Vec<String> = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--db" => {
                i += 1;
                db = rest.get(i).map(PathBuf::from);
            }
            "--store" => {
                i += 1;
                match rest.get(i).map(String::as_str) {
                    Some("catalog") => store = Store::Catalog,
                    Some("sidecar") => store = Store::Sidecar,
                    other => bail!("--store expects `catalog` or `sidecar`, got {other:?}"),
                }
            }
            "--sidecar-path" => {
                i += 1;
                sidecar_path = rest.get(i).map(PathBuf::from);
            }
            other => kept.push(other.to_string()),
        }
        i += 1;
    }
    *rest = kept;
    let db = db.ok_or_else(|| anyhow::anyhow!("--db PATH is required"))?;
    if matches!(store, Store::Sidecar) && sidecar_path.is_none() {
        // Default sidecar path: <db-parent>/cron.duckdb. Falls back to CWD if
        // `db` has no parent (e.g. bare filename).
        let parent = db.parent().unwrap_or_else(|| Path::new("."));
        sidecar_path = Some(parent.join("cron.duckdb"));
    }
    Ok(Common {
        db,
        store,
        sidecar_path,
    })
}

fn artifacts_and_preopens() -> Result<(ComponentArtifacts, Vec<(PathBuf, String)>)> {
    let artifacts = ComponentArtifacts::resolve_default()?;
    let extensions_dir = std::env::current_dir()?.join("artifacts/extensions");
    set_extension_root(extensions_dir);
    let cwd = std::env::current_dir()?;
    std::fs::create_dir_all(cwd.join(".duckdb/extension_data")).ok();
    let preopens = vec![(cwd, String::from("."))];
    Ok((artifacts, preopens))
}

/// Bring up a persistent driver-core state opened on `common.db`. If the
/// caller chose sidecar mode, ATTACH the sidecar file and USE it as the
/// current schema so bare `__cron_jobs` references resolve there. The two
/// cron extensions are LOADed once by `open_driver_core`; every subsequent
/// SQL runs against the same wasm instance, so `cron_next`, `cron_advance`,
/// `cron_due`, and the scheduler tables all stay resolvable without a
/// per-call header script.
fn open_state(
    engine: &Engine,
    artifacts: &ComponentArtifacts,
    preopens: &[(PathBuf, String)],
    common: &Common,
) -> Result<DriverCoreState> {
    let preopen_refs: Vec<(&Path, &str)> = preopens
        .iter()
        .map(|(h, g)| (h.as_path(), g.as_str()))
        .collect();
    let db_str = common.db.display().to_string();
    let mut state = open_driver_core(engine, artifacts, &preopen_refs, Some(&db_str))
        .with_context(|| format!("opening driver-core on {}", common.db.display()))?;
    if let Store::Sidecar = common.store {
        let path = common
            .sidecar_path
            .as_ref()
            .expect("sidecar path resolved by parse")
            .display()
            .to_string();
        let sql = format!(
            "ATTACH IF NOT EXISTS '{}' AS cron_sidecar; USE cron_sidecar;",
            sql_escape(&path)
        );
        driver_core_exec(&mut state, &sql)
            .map_err(|e| anyhow::anyhow!("cron: attach sidecar `{path}` failed: {e}"))?;
    }
    Ok(state)
}

// ---------------------------------------------------------------------------
// JSON emitters (replacements for the CLI's `.mode json`).
//
// `driver_core_query` returns each cell as a plain `String` (NULLs come back
// as empty strings), so we can't perfectly reproduce `.mode json`'s typed
// output (which distinguishes numbers from strings and `null` from `""`).
// The extension test at tests/cron_wasm_driver.rs only substring-matches
// `"name":"demo"` and `"last_status":"fired"`, and downstream tooling should
// parse JSON with a real parser, so we emit every value as a JSON string.
// Compact form (no whitespace) matches DuckDB's `.mode json`.
// ---------------------------------------------------------------------------

/// Serialize `rows` as a compact JSON array of `{col: val}` objects,
/// preserving column order (unlike `serde_json::Map` under BTreeMap
/// storage, which would sort them alphabetically).
fn rows_to_json(cols: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = String::from("[");
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            out.push(',');
        }
        out.push('{');
        for (i, col) in cols.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            // serde_json's Value::String Display impl handles escaping
            // (quotes, backslashes, control chars) properly.
            out.push_str(&serde_json::Value::String((*col).to_string()).to_string());
            out.push(':');
            let cell = row.get(i).cloned().unwrap_or_default();
            out.push_str(&serde_json::Value::String(cell).to_string());
        }
        out.push('}');
    }
    out.push(']');
    out
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn cmd_init(common: &Common) -> Result<()> {
    let (artifacts, preopens) = artifacts_and_preopens()?;
    let engine = build_engine_for_driver()?;
    let mut state = open_state(&engine, &artifacts, &preopens, common)?;
    // Run the bootstrap DDL. The extension itself commits it as
    // BOOTSTRAP_SQL; we keep an inline copy so the CLI can install it
    // without a self-referential `SELECT cron_bootstrap_sql()`.
    let bootstrap = include_bootstrap_sql();
    driver_core_exec(&mut state, bootstrap)
        .map_err(|e| anyhow::anyhow!("cron: bootstrap DDL failed: {e}"))?;
    eprintln!(
        "cron: initialized in {} ({}store)",
        common.db.display(),
        match common.store {
            Store::Catalog => "catalog ",
            Store::Sidecar => "sidecar ",
        }
    );
    Ok(())
}

fn cmd_schedule(common: &Common, rest: &[String]) -> Result<()> {
    let mut name: Option<String> = None;
    let mut expr: Option<String> = None;
    let mut sql: Option<String> = None;
    let mut catch_up = "skip".to_string();
    let mut nodename: Option<String> = None;
    let mut database: Option<String> = None;
    // --readonly wraps this job's fire in BEGIN; ... ROLLBACK; so writes
    // are undone. Best-effort (an explicit COMMIT in the SQL still commits)
    // — safety net for jobs the operator expects to be read-only.
    let mut readonly = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--catch-up" => {
                i += 1;
                catch_up = rest
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--catch-up expects a value"))?;
                if catch_up != "skip" && catch_up != "run_once" {
                    bail!("--catch-up expects `skip` or `run_once`, got `{catch_up}`");
                }
            }
            "--node" => {
                // Per-node targeting: this job only fires on a driver whose
                // resolved node identity == this exact string. Omit for
                // "any driver" (the default; `nodename IS NULL` in DB).
                i += 1;
                nodename = Some(
                    rest.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--node expects a name"))?,
                );
            }
            "--database" => {
                // Cross-catalog target (pg_cron parity: schedule_in_database).
                // The driver USEs this catalog for the job's SQL, then
                // restores. NAME must match an attached catalog at driver
                // startup (`cron run --attach PATH=NAME`). Omit for the
                // default "current catalog" behaviour (`database IS NULL`).
                i += 1;
                database = Some(
                    rest.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--database expects a name"))?,
                );
            }
            "--readonly" => readonly = true,
            other => {
                if name.is_none() {
                    name = Some(other.to_string());
                } else if expr.is_none() {
                    expr = Some(other.to_string());
                } else if sql.is_none() {
                    sql = Some(other.to_string());
                } else {
                    bail!("schedule: unexpected extra argument `{other}`");
                }
            }
        }
        i += 1;
    }
    let name = name.ok_or_else(|| anyhow::anyhow!("schedule: NAME is required"))?;
    let expr = expr.ok_or_else(|| anyhow::anyhow!("schedule: EXPR is required"))?;
    let sql = sql.ok_or_else(|| anyhow::anyhow!("schedule: SQL is required"))?;

    let (artifacts, preopens) = artifacts_and_preopens()?;
    let engine = build_engine_for_driver()?;
    let mut state = open_state(&engine, &artifacts, &preopens, common)?;

    // Idempotent upsert by `id = cron_id(name)`. Written out here rather than
    // as a macro because DuckDB scalar macros can't contain DML. `now()` (not
    // `current_timestamp`) is deliberate — `current_timestamp` gets bound as
    // a column reference inside `ON CONFLICT DO UPDATE SET`.
    let n = sql_escape(&name);
    let e = sql_escape(&expr);
    let s = sql_escape(&sql);
    let c = sql_escape(&catch_up);
    // Emit the nodename column as a SQL literal: 'x' when set, NULL when not.
    // Doing this at build time (instead of parameter-binding) keeps the whole
    // upsert one statement while `driver_core_exec` still takes a &str.
    let node_lit = match &nodename {
        Some(v) => format!("'{}'", sql_escape(v)),
        None => "NULL".to_string(),
    };
    // Same NULL-or-literal shape as nodename: NULL = run against the driver's
    // current catalog (backwards-compatible); a value = the driver USEs that
    // catalog before running `sql` and restores afterwards.
    let db_lit = match &database {
        Some(v) => format!("'{}'", sql_escape(v)),
        None => "NULL".to_string(),
    };
    // Readonly is a real BOOLEAN column, not the NULL/literal pattern used
    // for text opt-outs. `NULL` in the DB is treated as false at read time.
    let ro_lit = if readonly { "true" } else { "false" };
    let upsert = format!(
        "INSERT INTO __cron_jobs (id, name, schedule, sql, active, catch_up, next_run_at, nodename, database, readonly) \
         VALUES (cron_id('{n}'), '{n}', '{e}', '{s}', true, '{c}', \
         cron_next('{e}', epoch_ms(now())), {node_lit}, {db_lit}, {ro_lit}) \
         ON CONFLICT (id) DO UPDATE SET \
         schedule = excluded.schedule, sql = excluded.sql, catch_up = excluded.catch_up, \
         active = true, \
         next_run_at = cron_next(excluded.schedule, epoch_ms(now())), \
         nodename = excluded.nodename, \
         database = excluded.database, \
         readonly = excluded.readonly;"
    );
    driver_core_exec(&mut state, &upsert)
        .map_err(|e| anyhow::anyhow!("cron: schedule upsert failed: {e}"))?;

    let select = format!(
        "SELECT id, name, next_run_at, COALESCE(nodename, ''), COALESCE(database, ''), \
                COALESCE(readonly, false) \
         FROM __cron_jobs WHERE id = cron_id('{n}');"
    );
    let rows = driver_core_query(&mut state, &select)
        .map_err(|e| anyhow::anyhow!("cron: reading back schedule row: {e}"))?;
    println!(
        "{}",
        rows_to_json(
            &["id", "name", "next_run_at", "nodename", "database", "readonly"],
            &rows,
        )
    );
    Ok(())
}

fn cmd_alter(common: &Common, rest: &[String], op: &str) -> Result<()> {
    let target = rest
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{op}: NAME-OR-ID is required"))?;
    let (artifacts, preopens) = artifacts_and_preopens()?;
    let engine = build_engine_for_driver()?;
    let mut state = open_state(&engine, &artifacts, &preopens, common)?;
    let clause = match op {
        "unschedule" => {
            "DELETE FROM __cron_jobs WHERE id = TRY_CAST('{t}' AS BIGINT) OR name = '{t}'"
        }
        "activate" => {
            "UPDATE __cron_jobs SET active = true, \
             next_run_at = cron_next(schedule, epoch_ms(now())) \
             WHERE id = TRY_CAST('{t}' AS BIGINT) OR name = '{t}'"
        }
        "deactivate" => {
            "UPDATE __cron_jobs SET active = false \
             WHERE id = TRY_CAST('{t}' AS BIGINT) OR name = '{t}'"
        }
        _ => unreachable!(),
    }
    .replace("{t}", &sql_escape(&target));
    driver_core_exec(&mut state, &clause)
        .map_err(|e| anyhow::anyhow!("cron: {op} `{target}` failed: {e}"))?;
    eprintln!("cron: {op} `{target}`");
    Ok(())
}

fn cmd_list(common: &Common) -> Result<()> {
    let (artifacts, preopens) = artifacts_and_preopens()?;
    let engine = build_engine_for_driver()?;
    let mut state = open_state(&engine, &artifacts, &preopens, common)?;
    let rows = driver_core_query(
        &mut state,
        "SELECT id, name, schedule, active, catch_up, \
         next_run_at, last_run_at, last_status, last_error, \
         COALESCE(nodename, ''), COALESCE(database, ''), \
         COALESCE(readonly, false) \
         FROM __cron_jobs ORDER BY name;",
    )
    .map_err(|e| anyhow::anyhow!("cron: list failed: {e}"))?;
    let cols = [
        "id",
        "name",
        "schedule",
        "active",
        "catch_up",
        "next_run_at",
        "last_run_at",
        "last_status",
        "last_error",
        "nodename",
        "database",
        "readonly",
    ];
    println!("{}", rows_to_json(&cols, &rows));
    Ok(())
}

/// Emit Prometheus text-format counters + gauges from __cron_runs / __cron_jobs.
/// Text is written to stdout so operators can point a scrape endpoint at
/// `ducklink cron metrics --db X` (via a small HTTP shim) or pipe it directly
/// into pushgateway. No time-series storage — all metrics are computed on
/// each invocation from the underlying tables.
fn cmd_metrics(common: &Common) -> Result<()> {
    let (artifacts, preopens) = artifacts_and_preopens()?;
    let engine = build_engine_for_driver()?;
    let mut state = open_state(&engine, &artifacts, &preopens, common)?;

    // Per-(job, status) fire counts. `status` is derived from ok+err so a
    // succeeded job comes back as `fired`, a failed one as `failed`, matching
    // the __cron_jobs.last_status values. We aggregate over ALL history —
    // callers who want windowed metrics should trim __cron_runs first via
    // `cron run --history-limit N`.
    let per_status = driver_core_query(
        &mut state,
        "SELECT j.name, CASE WHEN r.ok THEN 'fired' ELSE 'failed' END, \
                COUNT(*), COALESCE(SUM(r.ms), 0) \
         FROM __cron_runs r \
         JOIN __cron_jobs j ON r.job_id = j.id \
         GROUP BY j.name, r.ok \
         ORDER BY j.name, r.ok DESC;",
    )
    .map_err(|e| anyhow::anyhow!("cron: metrics per-status query failed: {e}"))?;

    // Per-job last-fire timestamp (seconds since epoch, Prometheus's
    // canonical time unit for _timestamp gauges).
    let per_last = driver_core_query(
        &mut state,
        "SELECT name, COALESCE(last_run_at, 0) FROM __cron_jobs ORDER BY name;",
    )
    .map_err(|e| anyhow::anyhow!("cron: metrics last-run query failed: {e}"))?;

    // Whole-scheduler gauges.
    let active_rows = driver_core_query(
        &mut state,
        "SELECT COUNT(*) FROM __cron_jobs WHERE active;",
    )
    .map_err(|e| anyhow::anyhow!("cron: metrics active-count query failed: {e}"))?;
    let held_rows = driver_core_query(
        &mut state,
        &format!(
            "SELECT COUNT(*) FROM __cron_leases WHERE expires_at > {};",
            now_ms()
        ),
    )
    .map_err(|e| anyhow::anyhow!("cron: metrics leases query failed: {e}"))?;

    let active = active_rows
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_else(|| "0".to_string());
    let held = held_rows
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_else(|| "0".to_string());

    // Prometheus text format: HELP + TYPE lines, then one sample per line.
    // Label values are already alphanumeric+underscore in practice (cron_id
    // is a numeric hash and job names are user-picked but SQL-safe), but
    // escape just in case a user's job name contains a quote or backslash.
    let mut out = String::new();
    out.push_str("# HELP cron_fires_total Number of __cron_runs rows per (job, status).\n");
    out.push_str("# TYPE cron_fires_total counter\n");
    for row in &per_status {
        let job = escape_prom_label(row.first().map(String::as_str).unwrap_or(""));
        let status = row.get(1).map(String::as_str).unwrap_or("");
        let count = row.get(2).map(String::as_str).unwrap_or("0");
        out.push_str(&format!(
            "cron_fires_total{{job=\"{job}\",status=\"{status}\"}} {count}\n"
        ));
    }
    out.push_str("# HELP cron_fire_duration_ms_sum Total fire duration in ms per (job, status).\n");
    out.push_str("# TYPE cron_fire_duration_ms_sum counter\n");
    for row in &per_status {
        let job = escape_prom_label(row.first().map(String::as_str).unwrap_or(""));
        let status = row.get(1).map(String::as_str).unwrap_or("");
        let ms_sum = row.get(3).map(String::as_str).unwrap_or("0");
        out.push_str(&format!(
            "cron_fire_duration_ms_sum{{job=\"{job}\",status=\"{status}\"}} {ms_sum}\n"
        ));
    }
    out.push_str("# HELP cron_last_run_timestamp_seconds UTC epoch seconds of each job's last successful fire.\n");
    out.push_str("# TYPE cron_last_run_timestamp_seconds gauge\n");
    for row in &per_last {
        let job = escape_prom_label(row.first().map(String::as_str).unwrap_or(""));
        // last_run_at is ms; Prometheus wants seconds (float).
        let ms: i64 = row.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let secs = ms as f64 / 1000.0;
        out.push_str(&format!(
            "cron_last_run_timestamp_seconds{{job=\"{job}\"}} {secs}\n"
        ));
    }
    out.push_str("# HELP cron_active_jobs Number of active jobs in __cron_jobs.\n");
    out.push_str("# TYPE cron_active_jobs gauge\n");
    out.push_str(&format!("cron_active_jobs {active}\n"));
    out.push_str("# HELP cron_leases_held Number of non-expired per-job leases in __cron_leases.\n");
    out.push_str("# TYPE cron_leases_held gauge\n");
    out.push_str(&format!("cron_leases_held {held}\n"));
    print!("{out}");
    Ok(())
}

/// Prometheus label-value escape: backslash, double-quote, newline per
/// spec (openmetrics-text-format §5.1). Job names in this codebase are
/// unlikely to contain these but we don't control what users pick.
fn escape_prom_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn cmd_run(common: &Common, rest: &[String]) -> Result<()> {
    let mut interval_secs: u64 = 30;
    let mut once = false;
    let mut wasm_driver = false;
    let mut node_flag: Option<String> = None;
    // `--attach PATH=NAME` (repeatable). Each spec is turned into an
    // `ATTACH IF NOT EXISTS 'PATH' AS NAME;` at driver startup so
    // cross-catalog jobs (`--database NAME` at schedule time) can find
    // their target catalog when the tick loop USEs it.
    let mut attach_specs: Vec<(PathBuf, String)> = Vec::new();
    // `--history-limit N` — end-of-tick trim keeping only the N most-recent
    // __cron_runs rows per job. 0 (or unset) = no trim (v0 behaviour).
    let mut history_limit: u32 = 0;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--interval" => {
                i += 1;
                interval_secs = rest
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--interval expects a positive number"))?;
            }
            "--once" => once = true,
            // --wasm-driver: run cron-driver-tool.wasm instead of the native
            // Rust tick loop. Same behaviour + slightly better observability
            // (per-job error captured from exec()), but portable to any
            // wasi-preview2 host. See driver_exec.rs for the plan.
            "--wasm-driver" => wasm_driver = true,
            "--node" => {
                // Leader-election identity + per-node targeting: identifies
                // this driver in the __cron_leases table and matches
                // node-targeted jobs' `nodename` column.
                i += 1;
                node_flag = Some(
                    rest.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--node expects a name"))?,
                );
            }
            "--history-limit" => {
                i += 1;
                history_limit = rest
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--history-limit expects a non-negative number"))?;
            }
            "--attach" => {
                i += 1;
                let spec = rest
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--attach expects PATH=NAME"))?;
                let (path, name) = spec.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!("--attach expects PATH=NAME, got `{spec}`")
                })?;
                if path.is_empty() || name.is_empty() {
                    bail!("--attach expects a non-empty PATH and NAME, got `{spec}`");
                }
                attach_specs.push((PathBuf::from(path), name.to_string()));
            }
            other => bail!("run: unexpected argument `{other}`"),
        }
        i += 1;
    }
    let node = resolve_node_name(node_flag.as_deref());
    let (artifacts, preopens) = artifacts_and_preopens()?;
    if wasm_driver && !attach_specs.is_empty() {
        // Cross-catalog attach lives in the native tick loop's persistent
        // core connection; the wasm driver spawns its own process with its
        // own connection and can't see these attaches. Rather than silently
        // dropping them, refuse the combination so misconfiguration surfaces
        // immediately.
        bail!("run: --attach is not supported with --wasm-driver (v1)");
    }
    if wasm_driver {
        let tool = crate::driver_exec::default_tool_path()?;
        let preopen_refs: Vec<(&Path, &str)> = preopens
            .iter()
            .map(|(h, g)| (h.as_path(), g.as_str()))
            .collect();
        let mut extra: Vec<String> =
            vec!["--interval-secs".to_string(), interval_secs.to_string()];
        if once {
            extra.push("--once".to_string());
        }
        eprintln!(
            "cron: dispatching to wasm driver {} against {}",
            tool.display(),
            common.db.display()
        );
        let status = crate::driver_exec::run_driver_tool(
            &tool,
            &common.db,
            &artifacts,
            &preopen_refs,
            &extra,
        )?;
        if status.is_err() {
            std::process::exit(1);
        }
        return Ok(());
    }
    // Native tick loop: open the persistent core ONCE and reuse it across
    // ticks. Every SQL — the pre-advance UPDATE, the due-jobs SELECT, each
    // job's SQL, and the outcome-recording UPDATE / INSERT — runs through
    // the same wasm instance and the same DuckDB connection.
    let engine = build_engine_for_driver()?;
    let mut state = open_state(&engine, &artifacts, &preopens, common)?;
    // Attach any --attach PATH=NAME catalogs at driver startup. This is a
    // one-shot ATTACH IF NOT EXISTS per spec on the same persistent core
    // connection, so cross-catalog jobs (scheduled with `--database NAME`)
    // resolve to the right catalog when the tick loop USEs it. An attach
    // failure here is a startup-time misconfiguration (bad path, bad
    // permission, name clash) — bail so it's visible.
    for (path, name) in &attach_specs {
        let path_str = path.display().to_string();
        let attach_sql = format!(
            "ATTACH IF NOT EXISTS '{}' AS {};",
            sql_escape(&path_str),
            sql_escape_identifier(name),
        );
        driver_core_exec(&mut state, &attach_sql).map_err(|e| {
            anyhow::anyhow!("cron: attach `{}` as `{name}` failed: {e}", path.display())
        })?;
        eprintln!("cron: attached {} as {}", path.display(), name);
    }
    // Whole-tick lease TTL: 5x the tick interval, floored at 60s. Rationale:
    // covers a normal tick even with a slow SQL call plus process pause; a
    // crashed holder's lease expires naturally so the next driver takes over.
    let lease_ttl_ms: i64 = std::cmp::max(60_000, (interval_secs as i64).saturating_mul(5_000));
    eprintln!(
        "cron: ticking {} every {}s as node `{}` (Ctrl-C to stop)",
        common.db.display(),
        interval_secs,
        node
    );
    loop {
        // NOTE: a wasm trap inside `driver_core_exec` comes back as a
        // `String` prefixed with `driver-core: trap: `, so a per-job trap
        // is caught below and recorded as `last_status = 'failed'` on that
        // job. But the underlying core store may be poisoned by such a
        // trap, so subsequent ticks against the same `state` could fail
        // in cascade. Restarting the process (systemd, docker, etc.)
        // rebuilds a clean core — the loop keeps going otherwise so a
        // transient error doesn't tear down a long-running scheduler.
        let outcome = tick(&mut state, &node, lease_ttl_ms);
        match &outcome {
            Ok(TickOutcome { fired: 0, contended: 0 }) => {}
            Ok(TickOutcome { fired, contended: 0 }) => {
                eprintln!("cron: fired {fired} job(s)")
            }
            Ok(TickOutcome { fired: 0, contended }) => eprintln!(
                "cron: {contended} due job(s) already held by another driver; skipped"
            ),
            Ok(TickOutcome { fired, contended }) => eprintln!(
                "cron: fired {fired} job(s), {contended} already held by another driver"
            ),
            Err(err) => eprintln!("cron: tick error: {err:?}"),
        }
        // Trim __cron_runs if requested. Only run when we actually fired
        // something (a no-op tick can't have added new rows) and only trim
        // jobs that fired this tick — bounds the DELETE's work per tick.
        if history_limit > 0 {
            if let Ok(TickOutcome { fired: n, .. }) = outcome {
                if n > 0 {
                    trim_runs(&mut state, history_limit);
                }
            }
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval_secs));
    }
    Ok(())
}

/// Outcome of one tick — how many jobs this driver fired, plus how many
/// were skipped because another driver already held the per-job lease.
struct TickOutcome {
    fired: usize,
    contended: usize,
}

/// One tick: read due jobs, pre-advance next_run_at (so a mid-tick crash
/// doesn't re-fire the same window), then attempt to acquire a per-job
/// lease for each and fire the ones we won. Every SQL runs through
/// `state`'s persistent connection; `driver_core_exec` surfaces the real
/// DuckDB error text on failure so `last_status`/`last_error` reflect
/// the actual outcome.
///
/// Per-job leases (v1) — one row in `__cron_leases` per due job, keyed
/// `job:<id>`. If driver A's job takes 90s to run, driver B's tick can
/// still fire every OTHER due job during that window; only THAT job's
/// row is locked. This replaces v0's single `tick` key which was a
/// whole-scheduler barrier.
fn tick(state: &mut DriverCoreState, node: &str, lease_ttl_ms: i64) -> Result<TickOutcome> {
    let now = now_ms();
    tick_body(state, node, now, lease_ttl_ms)
}

/// The read-due → pre-advance → per-job-lease → fire loop. `now` is
/// captured by the caller so timing stays consistent between reads and
/// writes within one tick. `lease_ttl_ms` sets each per-job lease's TTL
/// so a crashed holder's row expires naturally on a later tick.
fn tick_body(
    state: &mut DriverCoreState,
    node: &str,
    now: i64,
    lease_ttl_ms: i64,
) -> Result<TickOutcome> {
    let node_sql = sql_escape(node);
    let expires_at = now.saturating_add(lease_ttl_ms);
    let mut contended = 0usize;
    let mut fired = 0usize;

    // 2. Read due jobs — the per-node variant, so both `nodename IS NULL`
    //    (global) and `nodename = <us>` (targeted) come back and no other
    //    node's jobs do. `driver_core_query` returns `Vec<Vec<String>>`
    //    directly. Also project `database` so cross-catalog jobs can be
    //    switched into their target catalog before firing.
    let read_sql = format!(
        "SELECT id, sql, COALESCE(database, ''), COALESCE(readonly, false) \
         FROM cron_due_for({now}, '{node_sql}');"
    );
    let rows = driver_core_query(state, &read_sql)
        .map_err(|e| anyhow::anyhow!("cron: reading due jobs: {e}"))?;
    if rows.is_empty() {
        return Ok(TickOutcome { fired: 0, contended: 0 });
    }
    let due: Vec<(u64, String, String, bool)> = rows
        .into_iter()
        .filter_map(|row| {
            let id = row.first()?.parse::<u64>().ok()?;
            let sql = row.get(1).cloned().unwrap_or_default();
            let database = row.get(2).cloned().unwrap_or_default();
            // DuckDB's driver_core_query stringifies booleans as "true"/"false".
            let readonly = row
                .get(3)
                .map(|s| s.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            Some((id, sql, database, readonly))
        })
        .collect();
    if due.is_empty() {
        return Ok(TickOutcome { fired: 0, contended: 0 });
    }

    // 3. Pre-advance `next_run_at` on every due row we're about to fire.
    //    Constrained by the same node filter so we don't accidentally
    //    advance another driver's targeted jobs (they might not be due
    //    from that other driver's perspective).
    let advance_sql = format!(
        "UPDATE __cron_jobs SET \
             next_run_at = cron_advance(schedule, next_run_at, {now}, catch_up), \
             last_run_at = {now} \
         WHERE active AND next_run_at IS NOT NULL AND next_run_at <= {now} \
           AND (nodename IS NULL OR nodename = '{node_sql}');"
    );
    driver_core_exec(state, &advance_sql)
        .map_err(|e| anyhow::anyhow!("cron: pre-advance failed: {e}"))?;

    // Cache the current catalog once per tick (a cheap `current_database()`),
    // but only if at least one due job actually targets a different catalog —
    // otherwise skip the round-trip. This is the anchor we restore to after
    // each cross-catalog job so subsequent per-tick bookkeeping
    // (record UPDATE / INSERT, lease release) always lands in the scheduler's
    // own catalog.
    let needs_switch = due.iter().any(|(_, _, db, _)| !db.is_empty());
    let original_catalog: Option<String> = if needs_switch {
        let rows = driver_core_query(state, "SELECT current_database();").map_err(|e| {
            anyhow::anyhow!("cron: reading current catalog: {e}")
        })?;
        rows.first().and_then(|r| r.first().cloned())
    } else {
        None
    };

    // 4. For each due job: acquire a per-job lease (skip if another
    //    driver already holds it), fire on the same persistent connection,
    //    then release. Independent per-job leases mean a slow job on
    //    driver A doesn't block driver B from firing other due jobs.
    for (id, sql, database, readonly) in &due {
        // If the job is readonly, wrap the fire in an explicit transaction
        // that ALWAYS ends in ROLLBACK. DuckDB executes a `;`-separated
        // batch as one call; if the user's SQL fails mid-way, the ROLLBACK
        // that follows still runs and unwinds any preceding work in the
        // transaction. Best-effort: an explicit COMMIT inside the user SQL
        // still commits (documented in --help).
        let effective_sql = if *readonly {
            format!("BEGIN TRANSACTION; {sql}; ROLLBACK;")
        } else {
            sql.clone()
        };
        // Per-job lease acquire. UPSERT semantics identical to the v0
        // whole-tick shape: insert on absent, overwrite on expired,
        // do-nothing if a valid lease is held by someone else. RETURNING
        // holder tells us whether WE ended up with it.
        let lease_key = format!("job:{id}");
        let acquire = format!(
            "INSERT INTO __cron_leases (key, holder, acquired_at, expires_at) \
             VALUES ('{lease_key}', '{node_sql}', {now}, {expires_at}) \
             ON CONFLICT (key) DO UPDATE SET \
                 holder = excluded.holder, \
                 acquired_at = excluded.acquired_at, \
                 expires_at = excluded.expires_at \
             WHERE __cron_leases.expires_at <= {now} \
             RETURNING holder;"
        );
        let acquire_rows = driver_core_query(state, &acquire)
            .map_err(|e| anyhow::anyhow!("cron: job {id} lease acquire failed: {e}"))?;
        let held_by_us = acquire_rows
            .first()
            .and_then(|r| r.first())
            .map(|h| h == node)
            .unwrap_or(false);
        if !held_by_us {
            contended += 1;
            continue;
        }

        let start = Instant::now();

        // Cross-catalog jobs: USE the target catalog, run the job SQL,
        // then ALWAYS restore the original catalog before recording the
        // outcome (so the record UPDATE / INSERT and the tick-lease
        // release land back in the scheduler's own catalog). Missing
        // catalogs are recorded as `skipped` without crashing the tick.
        let outcome: (Result<u64, String>, &'static str) = if database.is_empty() {
            (driver_core_exec(state, &effective_sql), "fired")
        } else {
            let use_sql = format!("USE {}", sql_escape_identifier(database));
            match driver_core_exec(state, &use_sql) {
                Err(_use_err) => {
                    let elapsed_ms = start.elapsed().as_millis() as i64;
                    let msg = format!("cron: database {database} is not attached");
                    let err_lit = format!("'{}'", msg.replace('\'', "''"));
                    let record = format!(
                        "UPDATE __cron_jobs SET last_status = 'skipped', last_error = {err_lit} WHERE id = {id};\n\
                         INSERT INTO __cron_runs VALUES ({id}, {now}, false, {err_lit}, {elapsed_ms});"
                    );
                    if let Err(e) = driver_core_exec(state, &record) {
                        eprintln!("cron: could not record skip for job {id}: {e}");
                    }
                    continue;
                }
                Ok(_) => {
                    let result = driver_core_exec(state, &effective_sql);
                    // Unconditional restore — even on job error — so the
                    // lease-release UPDATE and the outcome-recording SQL
                    // always land in the scheduler's own catalog.
                    if let Some(orig) = &original_catalog {
                        let restore = format!("USE {}", sql_escape_identifier(orig));
                        if let Err(e) = driver_core_exec(state, &restore) {
                            eprintln!(
                                "cron: could not restore catalog to {orig} after job {id}: {e}"
                            );
                        }
                    }
                    (result, "fired")
                }
            }
        };
        let elapsed_ms = start.elapsed().as_millis() as i64;
        let (ok, err_lit, status) = match &outcome.0 {
            Ok(_) => (true, "NULL".to_string(), outcome.1),
            Err(msg) => {
                let escaped = msg.replace('\'', "''");
                (false, format!("'{escaped}'"), "failed")
            }
        };
        let record = format!(
            "UPDATE __cron_jobs SET last_status = '{status}', last_error = {err} WHERE id = {id};\n\
             INSERT INTO __cron_runs VALUES ({id}, {now}, {ok}, {err}, {ms});",
            err = err_lit,
            ok = ok,
            ms = elapsed_ms,
        );
        if let Err(e) = driver_core_exec(state, &record) {
            eprintln!("cron: could not record run for job {id}: {e}");
        }

        // Per-job lease release. Best-effort: TTL would eventually
        // free it. Only release when WE still hold it (a super-slow
        // job could have gotten stolen).
        let release = format!(
            "UPDATE __cron_leases SET expires_at = 0 \
             WHERE key = '{lease_key}' AND holder = '{node_sql}';"
        );
        if let Err(e) = driver_core_exec(state, &release) {
            eprintln!("cron: could not release job {id} lease: {e}");
        }

        fired += 1;
    }

    Ok(TickOutcome { fired, contended })
}

/// Delete every `__cron_runs` row beyond the `limit` most-recent per
/// `job_id`. Runs at end of tick when `--history-limit N` is set and any
/// jobs actually fired. Best-effort: any error is logged and the loop
/// keeps going — a trim failure never kills a driver.
///
/// The DELETE uses `ROW_NUMBER() OVER (PARTITION BY job_id ORDER BY run_at
/// DESC)` so multi-job DBs get bounded per-job. Not indexed on `(job_id,
/// run_at)` today — call sites keep history_limit large enough that the
/// window doesn't grow beyond a table scan's tolerable cost.
fn trim_runs(state: &mut DriverCoreState, limit: u32) {
    let sql = format!(
        "DELETE FROM __cron_runs \
         WHERE rowid IN ( \
            SELECT rowid FROM ( \
                SELECT rowid, row_number() OVER (PARTITION BY job_id ORDER BY run_at DESC) AS rn \
                FROM __cron_runs \
            ) t \
            WHERE t.rn > {limit} \
         );"
    );
    if let Err(e) = driver_core_exec(state, &sql) {
        eprintln!("cron: __cron_runs trim (--history-limit {limit}) failed: {e}");
    }
}

/// The DDL text baked into the extension. We prefer running it via the
/// extension's `cron_bootstrap_sql()` scalar (single source of truth), but
/// the CLI can't `SELECT cron_bootstrap_sql()` and pipe the result back into
/// itself in the same session — so keep an inline copy that matches. If they
/// ever drift, the smoke test catches it.
fn include_bootstrap_sql() -> &'static str {
    // Kept in sync with extensions/cron_scheduler-component/src/lib.rs
    // (BOOTSTRAP_SQL). The extension's smoke test asserts the tables +
    // macros exist; the CLI needs its own copy because it runs BEFORE the
    // extension's `cron_bootstrap_sql()` scalar is available in-transaction.
    r#"
CREATE TABLE IF NOT EXISTS __cron_jobs (
    id           UBIGINT      PRIMARY KEY,
    name         TEXT         NOT NULL UNIQUE,
    schedule     TEXT         NOT NULL,
    sql          TEXT         NOT NULL,
    active       BOOLEAN      NOT NULL DEFAULT true,
    catch_up     TEXT         NOT NULL DEFAULT 'skip',
    next_run_at  BIGINT,
    last_run_at  BIGINT,
    last_status  TEXT,
    last_error   TEXT,
    created_at   BIGINT       NOT NULL DEFAULT (epoch_ms(now())),
    nodename     TEXT,
    database     TEXT,
    readonly     BOOLEAN
);
ALTER TABLE __cron_jobs ADD COLUMN IF NOT EXISTS nodename TEXT;
ALTER TABLE __cron_jobs ADD COLUMN IF NOT EXISTS database TEXT;
ALTER TABLE __cron_jobs ADD COLUMN IF NOT EXISTS readonly BOOLEAN;
CREATE TABLE IF NOT EXISTS __cron_runs (
    job_id   UBIGINT NOT NULL,
    run_at   BIGINT NOT NULL,
    ok       BOOLEAN NOT NULL,
    err      TEXT,
    ms       BIGINT
);
CREATE TABLE IF NOT EXISTS __cron_leases (
    key         TEXT     NOT NULL PRIMARY KEY,
    holder      TEXT     NOT NULL,
    acquired_at BIGINT   NOT NULL,
    expires_at  BIGINT   NOT NULL
);
CREATE OR REPLACE MACRO cron_id(name) AS hash(name);
CREATE OR REPLACE MACRO cron_due(now_ms) AS TABLE
    SELECT id, name, schedule, sql, catch_up, next_run_at, last_run_at, nodename, database, readonly
    FROM __cron_jobs
    WHERE active AND next_run_at IS NOT NULL AND next_run_at <= now_ms AND nodename IS NULL
    ORDER BY next_run_at;
CREATE OR REPLACE MACRO cron_due_for(now_ms, node) AS TABLE
    SELECT id, name, schedule, sql, catch_up, next_run_at, last_run_at, nodename, database, readonly
    FROM __cron_jobs
    WHERE active AND next_run_at IS NOT NULL AND next_run_at <= now_ms
      AND (nodename IS NULL OR nodename = node)
    ORDER BY next_run_at;
"#
}

// ---------------------------------------------------------------------------
// Entry point (dispatched from ducklink.rs)
// ---------------------------------------------------------------------------

pub fn run(args: &[String]) -> Result<()> {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("{HELP}");
        std::process::exit(2);
    };
    if matches!(sub, "-h" | "--help" | "help") {
        eprintln!("{HELP}");
        return Ok(());
    }
    let mut rest: Vec<String> = args[1..].to_vec();
    let common = parse_common(&mut rest)?;
    match sub {
        "init" => cmd_init(&common),
        "schedule" => cmd_schedule(&common, &rest),
        "unschedule" => cmd_alter(&common, &rest, "unschedule"),
        "activate" => cmd_alter(&common, &rest, "activate"),
        "deactivate" => cmd_alter(&common, &rest, "deactivate"),
        "list" => cmd_list(&common),
        "metrics" => cmd_metrics(&common),
        "run" => cmd_run(&common, &rest),
        other => {
            eprintln!("unknown subcommand `{other}`\n\n{HELP}");
            std::process::exit(2);
        }
    }
}
