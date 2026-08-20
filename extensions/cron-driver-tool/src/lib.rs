//! cron_scheduler driver, as a wasi:cli/run tool.
//!
//! The tick loop that used to live in the native Rust `ducklink cron run`
//! moved here as portable wasm. The host implements the small
//! `duckdb:driver/exec` bridge (open + exec + query on a real DuckDB
//! connection) and the tool imports `wasi:clocks/monotonic-clock` +
//! `wasi:io/poll` to sleep between ticks with a wasi-native pollable — no
//! busy-wait.
//!
//! Usage:  cron-driver-tool <db-path> [--interval-secs N] [--once]
//!         (argv[0] is dropped by convention; the DB path is required)
//!
//! Semantics match the native driver 1:1: read due jobs via `cron_due(now)`,
//! pre-advance every `next_run_at` before firing, run each job's SQL and
//! append a `__cron_runs` row, sleep, repeat.

mod bindings;

use bindings::duckdb::driver::exec as sql;
use bindings::wasi::cli::environment;
use bindings::wasi::cli::stderr;
use bindings::wasi::clocks::monotonic_clock;
use bindings::wasi::clocks::wall_clock;

struct Component;

impl bindings::exports::wasi::cli::run::Guest for Component {
    fn run() -> Result<(), ()> {
        if let Err(err) = drive() {
            let _ = writeln_stderr(&format!("cron-driver-tool: {err}"));
            Err(())
        } else {
            Ok(())
        }
    }
}

bindings::exports::wasi::cli::run::__export_wasi_cli_run_0_2_6_cabi!(
    Component with_types_in bindings::exports::wasi::cli::run
);

fn drive() -> Result<(), String> {
    let (db, interval_secs, once) = parse_args()?;
    let conn = sql::open(&db).map_err(|e| format!("open {db}: {e}"))?;

    // Bootstrap: LOAD both extensions once so subsequent SQL sees the
    // scalars + the cron_due table macro.
    conn.exec("LOAD cron; LOAD cron_scheduler;")
        .map_err(|e| format!("bootstrap load: {e}"))?;

    let interval_ns: u64 = interval_secs.saturating_mul(1_000_000_000);
    log(&format!(
        "cron-driver-tool: ticking `{db}` every {interval_secs}s ({} mode)",
        if once { "once" } else { "loop" }
    ));

    loop {
        match tick(&conn) {
            Ok(0) => {}
            Ok(n) => log(&format!("cron-driver-tool: fired {n} job(s)")),
            Err(e) => log(&format!("cron-driver-tool: tick error: {e}")),
        }
        if once {
            break Ok(());
        }
        // Real wasi sleep: subscribe to a monotonic-clock duration and block
        // on the returned pollable via wasi:io/poll. The host's event loop
        // wakes us when the duration elapses.
        let p = monotonic_clock::subscribe_duration(interval_ns);
        p.block();
    }
}

fn tick(conn: &sql::Connection) -> Result<usize, String> {
    let now = wall_clock_ms();

    // 1. Read due jobs. `query()` returns stringified cells; cron_due
    //    projects (id, name, schedule, sql, catch_up, next_run_at, last_run_at).
    let rows = conn
        .query(&format!("SELECT id, sql FROM cron_due({now});"))
        .map_err(|e| format!("read due: {e}"))?;
    if rows.is_empty() {
        return Ok(0);
    }

    // 2. Pre-advance every due row BEFORE we fire (skip-semantics: a crash
    //    mid-tick doesn't repeat the same window on the next tick).
    conn.exec(&format!(
        "UPDATE __cron_jobs SET \
           next_run_at = cron_advance(schedule, next_run_at, {now}, catch_up), \
           last_run_at = {now} \
         WHERE active AND next_run_at IS NOT NULL AND next_run_at <= {now};"
    ))
    .map_err(|e| format!("pre-advance: {e}"))?;

    // 3. Fire each job. `conn.exec()` swallows both stdout AND stderr into
    //    its result — Err(msg) means the SQL failed — so unlike the native
    //    driver we get real per-job success/failure here for free.
    let mut fired = 0usize;
    for row in &rows {
        let id: u64 = row
            .first()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("row missing id: {row:?}"))?;
        let sql: &str = row.get(1).map(String::as_str).unwrap_or("");
        let t0 = monotonic_clock::now();
        let (ok, err_lit) = match conn.exec(sql) {
            Ok(_) => (true, "NULL".to_string()),
            Err(msg) => (false, format!("'{}'", msg.replace('\'', "''"))),
        };
        let elapsed_ms = (monotonic_clock::now().saturating_sub(t0)) / 1_000_000;
        let record = format!(
            "UPDATE __cron_jobs SET last_status = '{status}', last_error = {err} WHERE id = {id}; \
             INSERT INTO __cron_runs VALUES ({id}, {now}, {ok}, {err}, {ms});",
            status = if ok { "fired" } else { "failed" },
            err = err_lit,
            ms = elapsed_ms,
        );
        if let Err(e) = conn.exec(&record) {
            log(&format!("cron-driver-tool: could not record run {id}: {e}"));
        }
        fired += 1;
    }

    Ok(fired)
}

fn parse_args() -> Result<(String, u64, bool), String> {
    let args = environment::get_arguments();
    let mut db: Option<String> = None;
    let mut interval: u64 = 30;
    let mut once = false;
    let mut it = args.into_iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--interval-secs" => {
                interval = it
                    .next()
                    .ok_or_else(|| "--interval-secs expects a value".to_string())?
                    .parse()
                    .map_err(|e| format!("--interval-secs: {e}"))?;
            }
            "--once" => once = true,
            other if !other.starts_with("--") && db.is_none() => db = Some(other.to_string()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let db =
        db.ok_or_else(|| "usage: cron-driver-tool <db> [--interval-secs N] [--once]".to_string())?;
    Ok((db, interval, once))
}

fn wall_clock_ms() -> u64 {
    let now = wall_clock::now();
    now.seconds
        .saturating_mul(1_000)
        .saturating_add((now.nanoseconds / 1_000_000) as u64)
}

fn log(msg: &str) {
    let _ = writeln_stderr(msg);
}

fn writeln_stderr(msg: &str) -> Result<(), String> {
    use bindings::wasi::io::streams::StreamError;
    let s = stderr::get_stderr();
    let mut buf = msg.to_string();
    buf.push('\n');
    let bytes = buf.as_bytes();
    let mut written = 0;
    while written < bytes.len() {
        match s.blocking_write_and_flush(&bytes[written..]) {
            Ok(()) => written = bytes.len(),
            Err(StreamError::Closed) => return Ok(()),
            Err(StreamError::LastOperationFailed(e)) => {
                return Err(format!("stderr write: {}", e.to_debug_string()))
            }
        }
    }
    Ok(())
}
