//! `cron_scheduler` — cron-style SQL scheduler for DuckDB.
//!
//! # Design in one paragraph
//!
//! The whole scheduler is pure SQL over two tables (`__cron_jobs`,
//! `__cron_runs`) plus a driver loop. The driver — either the CLI
//! (`ducklink cron run`) or any host embedder running its own timer —
//! executes `SELECT cron_run_due(now())` on a tick, which returns the
//! number of jobs it fired. That's it: no threads inside wasm (impossible),
//! no host-specific IPC.
//!
//! This wasm extension is the small helper layer that the tables + macros
//! depend on:
//!
//!   * `cron_advance(schedule, last_ms, now_ms, catch_up)` — next fire time
//!     for a job that just ran (or missed windows), honouring `catch_up`
//!     policy: `'skip'` jumps to the next fire strictly after `now_ms`;
//!     `'run_once'` re-fires immediately if we're already past the previously
//!     scheduled window.
//!   * `cron_bootstrap_sql()` — returns the entire DDL as a string so a
//!     host embedder can `SELECT cron_bootstrap_sql()` and run the result.
//!     The CLI runs the same string automatically on `ducklink cron init`.
//!
//! The bootstrap DDL creates the tables + the `cron_schedule` /
//! `cron_unschedule` / `cron_activate` / `cron_deactivate` / `cron_run_due`
//! scalar macros. Storage location is a settings pair:
//!
//! ```sql
//! -- default: tables live in the current catalog
//! SET cron_scheduler.store = 'catalog';
//! -- alternative: ATTACH a sidecar db and put the tables there
//! SET cron_scheduler.store = 'sidecar';
//! SET cron_scheduler.sidecar_path = '~/.ducklink/cron.duckdb';
//! ```
//!
//! Both are supported by the CLI (`--store catalog|sidecar`, `--sidecar-path
//! PATH`). The bootstrap SQL emits the same table shape in either case.

extern crate alloc;

use alloc::string::String;
use chrono::{DateTime, TimeZone, Utc};
use core::str::FromStr;
use croner::Cron;
use datalink_extcore::NeutralValue;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

/// The bootstrap SQL, returned verbatim by `cron_bootstrap_sql()` and run by
/// `ducklink cron init`. Idempotent (all `CREATE ... IF NOT EXISTS` /
/// `CREATE OR REPLACE MACRO`).
///
/// The `cron_next` / `cron_prev` / `cron_is_valid` scalars come from the
/// separate `cron` extension (`cron-component`); load that first (the CLI
/// does automatically).
const BOOTSTRAP_SQL: &str = r#"
-- __cron_jobs: one row per scheduled job.
--   catch_up: 'skip' | 'run_once' (see cron_advance in cron_scheduler ext).
--   id     : stable hash(name)::BIGINT so re-scheduling by name is idempotent.
--   next_run_at / last_run_at : UTC ms (epoch_ms). NULL next_run_at = never fire.
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
    -- Per-node targeting: NULL = fires on any driver; non-NULL = fires only
    -- when the ticking driver's node identity matches this exact string.
    nodename     TEXT,
    -- Cross-catalog target (pg_cron parity: schedule_in_database). NULL =
    -- run against the driver's current catalog; non-NULL = the driver USEs
    -- this catalog for the job's SQL, then restores. The catalog must be
    -- attached to the driver at startup (`ducklink cron run --attach
    -- PATH=NAME`); missing catalogs are recorded as `last_status='skipped'`.
    database     TEXT
);

-- Additive migration path for DBs init'd before `nodename` existed: DuckDB
-- supports `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (14.0+), which is a
-- no-op the second time and later. Kept OUTSIDE the CREATE TABLE so it also
-- runs against a pre-existing table.
ALTER TABLE __cron_jobs ADD COLUMN IF NOT EXISTS nodename TEXT;
ALTER TABLE __cron_jobs ADD COLUMN IF NOT EXISTS database TEXT;

-- __cron_runs: append-only history (bounded by callers; no auto-trim in v0).
CREATE TABLE IF NOT EXISTS __cron_runs (
    job_id   UBIGINT NOT NULL,
    run_at   BIGINT NOT NULL,
    ok       BOOLEAN NOT NULL,
    err      TEXT,
    ms       BIGINT
);

-- __cron_leases: at most one row per lease key. The driver's tick loop uses
-- the single key 'tick' as a whole-scheduler mutex — a driver acquires it
-- with a TTL at tick start, holds it while it reads-due / pre-advances /
-- fires, then releases. Contending drivers whose acquire fails skip that
-- tick with a log line. A crashed holder's lease expires naturally at
-- `expires_at`; the next tick's acquire takes it over.
CREATE TABLE IF NOT EXISTS __cron_leases (
    key         TEXT     NOT NULL PRIMARY KEY,
    holder      TEXT     NOT NULL,
    acquired_at BIGINT   NOT NULL,
    expires_at  BIGINT   NOT NULL
);

-- Read-only helpers. DuckDB scalar macros are expression-only (no DML), so
-- INSERT / UPDATE / DELETE against __cron_jobs are driven by the CLI or by
-- the host embedder writing plain SQL — the shapes are documented in
-- `ducklink cron --help`.

-- cron_id(name) -> stable UBIGINT id used as the __cron_jobs primary key.
-- (hash() returns UBIGINT; keep it as-is so the value doesn't overflow.)
CREATE OR REPLACE MACRO cron_id(name) AS
    hash(name);

-- cron_due(now_ms) -> table of jobs whose next_run_at <= now_ms AND that are
-- either global (nodename IS NULL) or match the current driver's identity.
-- The single-arg form kept for backward compatibility: it fires only global
-- jobs (a driver that has no node identity of its own).
CREATE OR REPLACE MACRO cron_due(now_ms) AS TABLE
    SELECT id, name, schedule, sql, catch_up, next_run_at, last_run_at, nodename, database
    FROM __cron_jobs
    WHERE active
      AND next_run_at IS NOT NULL
      AND next_run_at <= now_ms
      AND nodename IS NULL
    ORDER BY next_run_at;

-- cron_due_for(now_ms, node) -> the per-node variant. `node` is the ticking
-- driver's identity (see `resolve_node_name` in ducklink-host). Fires jobs
-- that are global (nodename IS NULL) OR node-targeted at THIS driver.
CREATE OR REPLACE MACRO cron_due_for(now_ms, node) AS TABLE
    SELECT id, name, schedule, sql, catch_up, next_run_at, last_run_at, nodename, database
    FROM __cron_jobs
    WHERE active
      AND next_run_at IS NOT NULL
      AND next_run_at <= now_ms
      AND (nodename IS NULL OR nodename = node)
    ORDER BY next_run_at;
"#;

/// Deterministic UTC cron parse (matches cron-component).
fn parse_cron(expr: &str) -> Option<Cron> {
    Cron::from_str(expr).ok()
}

/// UTC ms -> DateTime.
fn ms_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    match Utc.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => Some(dt),
        _ => None,
    }
}

/// Advance policy. `last_ms` is the timestamp of the run we just performed
/// (or the previously scheduled fire time we missed); `now_ms` is the
/// wall-clock at the call site. Semantics:
///
///   * `'skip'` (default) — return the next fire strictly after `now_ms`.
///     Missed windows collapse: pg_cron parity.
///   * `'run_once'` — if the next fire after `last_ms` is at-or-before
///     `now_ms`, return `now_ms` (fire again immediately, once, to catch up).
///     Only ONE catch-up run happens per missed window.
///
/// Any invalid input -> NULL (the caller keeps `next_run_at` as-is).
fn advance(schedule: &str, last_ms: i64, now_ms: i64, catch_up: &str) -> Option<i64> {
    let cron = parse_cron(schedule)?;
    let last_dt = ms_to_dt(last_ms)?;
    let next_after_last = cron.find_next_occurrence(&last_dt, false).ok()?;
    let next_ms = next_after_last.timestamp_millis();

    match catch_up {
        "run_once" if next_ms <= now_ms => Some(now_ms),
        "skip" | "run_once" => {
            if next_ms > now_ms {
                Some(next_ms)
            } else {
                let now_dt = ms_to_dt(now_ms)?;
                let next_after_now = cron.find_next_occurrence(&now_dt, false).ok()?;
                Some(next_after_now.timestamp_millis())
            }
        }
        _ => None,
    }
}

datalink_extcore::declare! {
    core = CronSchedulerCore;
    extension = "cron_scheduler";
    version = env!("CARGO_PKG_VERSION");

    scalar cron_advance(text, int64, int64, text) -> int64 [propagate, deterministic] = |args| {
        let schedule = args.arg_text(0, "cron_advance")?;
        let last_ms  = args.arg_int(1, "cron_advance")?;
        let now_ms   = args.arg_int(2, "cron_advance")?;
        let policy   = args.arg_text(3, "cron_advance")?;
        Ok(match advance(&schedule, last_ms, now_ms, &policy) {
            Some(v) => NeutralValue::Int64(v),
            None => NeutralValue::Null,
        })
    };

    scalar cron_bootstrap_sql() -> text [propagate, deterministic] = |_args| {
        Ok(NeutralValue::Text(String::from(BOOTSTRAP_SQL)))
    };
}

datalink_extcore::duckdb_shim! {
    core = CronSchedulerCore;
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    runtime = duckdb::extension::runtime;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    guest = exports::duckdb::extension::guest;
    export = export;
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    // 2023-11-14T22:13:20Z; daily-midnight next fire = 2023-11-15T00:00:00Z.
    const REF_MS: i64 = 1_700_000_000_000;
    const NEXT_MIDNIGHT_MS: i64 = 1_700_006_400_000;

    #[test]
    fn advance_skip_normal() {
        // last_run just ran at REF_MS-1h; now = REF_MS; next fire is the next
        // scheduled midnight after now, not after last (they happen to match).
        assert_eq!(
            advance("0 0 * * *", REF_MS - 3_600_000, REF_MS, "skip"),
            Some(NEXT_MIDNIGHT_MS)
        );
    }

    #[test]
    fn advance_skip_collapses_missed_windows() {
        // last_run was days ago; now is REF_MS; skip goes to the next fire
        // AFTER now, not the next after last.
        assert_eq!(
            advance("0 0 * * *", REF_MS - 3 * 86_400_000, REF_MS, "skip"),
            Some(NEXT_MIDNIGHT_MS)
        );
    }

    #[test]
    fn advance_run_once_catches_up() {
        // last_run was days ago; run_once fires immediately (returns now_ms).
        assert_eq!(
            advance("0 0 * * *", REF_MS - 3 * 86_400_000, REF_MS, "run_once"),
            Some(REF_MS)
        );
    }

    #[test]
    fn advance_run_once_normal() {
        // last_run just ran; the next fire is in the future; run_once behaves
        // like skip and returns that.
        assert_eq!(
            advance("0 0 * * *", REF_MS - 3_600_000, REF_MS, "run_once"),
            Some(NEXT_MIDNIGHT_MS)
        );
    }

    #[test]
    fn advance_invalid_expr() {
        assert_eq!(advance("garbage", REF_MS, REF_MS, "skip"), None);
    }

    #[test]
    fn advance_invalid_policy() {
        assert_eq!(advance("0 0 * * *", REF_MS, REF_MS, "bogus"), None);
    }

    #[test]
    fn bootstrap_sql_is_nonempty_and_mentions_tables() {
        assert!(BOOTSTRAP_SQL.contains("__cron_jobs"));
        assert!(BOOTSTRAP_SQL.contains("__cron_runs"));
        assert!(BOOTSTRAP_SQL.contains("__cron_leases"));
        assert!(BOOTSTRAP_SQL.contains("cron_due"));
        assert!(BOOTSTRAP_SQL.contains("cron_due_for"));
        assert!(BOOTSTRAP_SQL.contains("cron_id"));
        assert!(BOOTSTRAP_SQL.contains("nodename"));
        assert!(BOOTSTRAP_SQL.contains("database"));
    }
}
