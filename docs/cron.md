# `ducklink cron` — SQL scheduler for DuckDB

## Overview

`ducklink cron` is a pg_cron-shaped scheduler that lives entirely inside
DuckDB. Two tables (`__cron_jobs`, `__cron_runs`) plus a set of macros describe
every scheduled job; a *driver* ticks on an interval, reads the rows whose
`next_run_at` has arrived, executes each row's `sql`, and records the outcome.
Nothing runs in the background inside DuckDB itself — the driver (the CLI, a
wasm tool, or your own timer) is what turns time into work.

- **A job only fires while a driver is running.** No driver, no ticks.
- **Missed windows are handled per-job by [`catch_up`](#catch-up-policies)** —
  collapse (`skip`) or fire once immediately (`run_once`).

## Quickstart

```bash
# 1. Create the cron tables + macros inside app.duckdb.
ducklink cron init --db app.duckdb

# 2. Schedule a job by name. Re-running with the same name updates in place.
ducklink cron schedule --db app.duckdb 'nightly-vacuum' '0 3 * * *' 'CHECKPOINT'

# 3. Start the driver. It reads due jobs, fires them, sleeps, repeats.
ducklink cron run --db app.duckdb --interval 30
```

`init` seeds the DDL. `schedule` upserts a job keyed by name (idempotent).
`run` is a long-lived process — see [Deployment recipes](#deployment-recipes).

## Command reference

Every subcommand takes `--db PATH` (required — cron over `:memory:` is
pointless), plus the flags described in [Storage modes](#storage-modes).

### `init`

```bash
ducklink cron init --db app.duckdb
```

Installs `__cron_jobs`, `__cron_runs`, and the cron macros. Idempotent.

### `schedule <NAME> <EXPR> <SQL>`

```bash
ducklink cron schedule --db app.duckdb \
  --catch-up run_once \
  'hourly-rollup' '0 * * * *' \
  "INSERT INTO rollup SELECT * FROM raw WHERE ts >= now() - INTERVAL 1 HOUR"
```

Upserts a job keyed by `NAME`. `EXPR` is a standard 5-field cron expression
evaluated in UTC. `SQL` is any DuckDB statement (or `;`-separated batch).
`--catch-up skip|run_once` sets the missed-window policy (default `skip`).
`--node NAME` targets the job at a specific driver (see
[Running multiple drivers](#running-multiple-drivers)); omit for the default
"any driver" behaviour. `--database NAME` runs the job against a specific
attached catalog (see [Cross-catalog jobs](#cross-catalog-jobs)); omit to run
against the driver's current catalog (the backwards-compatible default).

### `unschedule <NAME-OR-ID>` / `activate <NAME-OR-ID>` / `deactivate <NAME-OR-ID>`

```bash
ducklink cron unschedule --db app.duckdb 'nightly-vacuum'
ducklink cron deactivate --db app.duckdb 'nightly-vacuum'
ducklink cron activate   --db app.duckdb 'nightly-vacuum'
```

Delete, pause, and re-arm respectively. `activate` recomputes `next_run_at`
from now, so a paused job never fires a catch-up burst on re-enable.

### `list`

```bash
ducklink cron list --db app.duckdb
```

Prints every scheduled job as JSON.

### `run`

```bash
ducklink cron run --db app.duckdb --interval 60
ducklink cron run --db app.duckdb --once           # single tick, then exit
ducklink cron run --db app.duckdb --wasm-driver    # portable wasi driver
```

`--interval SECS` (default `30`) is the sleep between ticks. `--once` fires
what is due and exits — useful when an external scheduler (systemd timer,
Cloudflare cron trigger) supplies the cadence. `--wasm-driver` runs the
portable wasi driver — see [Driver kinds](#driver-kinds). `--node NAME`
identifies this driver for leader election + per-node targeting — see
[Running multiple drivers](#running-multiple-drivers). Precedence for the
identity: `--node` > `$DUCKLINK_CRON_NODE` > `<hostname>-<pid>` fallback.
`--attach PATH=NAME` (repeatable) runs
`ATTACH IF NOT EXISTS 'PATH' AS NAME` at driver startup so cross-catalog jobs
scheduled with `--database NAME` can find their target catalog — see
[Cross-catalog jobs](#cross-catalog-jobs).

## Catch-up policies

Each job's `catch_up` value decides what happens when a driver was offline
during one or more scheduled windows.

| Policy | Behaviour |
| --- | --- |
| `skip` (default) | pg_cron parity. Missed windows collapse; next fire is the next scheduled mark strictly after `now`. |
| `run_once` | Fire once immediately to catch up on a missed window, then resume normal cadence. Only ONE catch-up run per gap. |

Example — job scheduled `*/5 * * * *`, driver down for 2 hours:

- Under `skip`, the job fires at the *next* 5-minute mark. The ~24 missed
  windows do not fire.
- Under `run_once`, the job fires immediately, then resumes 5-minute cadence.

`next_run_at` is advanced *before* the job's SQL runs, so a mid-tick crash
does not re-fire the same window on the next tick.

## Storage modes

- `--store catalog` (default) — the cron tables live inside the DB passed with
  `--db`. Simplest setup; the schedule travels with the DB.
- `--store sidecar` — the tables live in a separate ATTACHed DB, path
  controlled by `--sidecar-path` (default `<db-dir>/cron.duckdb`).

Pick `sidecar` to keep cron metadata out of a business-data catalog, or to
coordinate jobs across multiple databases from one cron file — point every
driver at the same `--sidecar-path`.

```bash
ducklink cron init --db warehouse.duckdb \
    --store sidecar --sidecar-path /var/lib/ducklink/cron.duckdb
```

## Cross-catalog jobs

Each job may target a specific catalog other than the one holding the
scheduler tables (pg_cron's `cron.schedule_in_database()` shape). This lets
you keep cron metadata in a dedicated `cron.duckdb` while jobs write to
`app.duckdb` — the scheduler and the workload live in separate files.

Two moving parts:

- **At schedule time**, add `--database NAME` — the job records that catalog
  name in `__cron_jobs.database`. Omit the flag to keep the historical
  behaviour: the job runs against the driver's current catalog.
- **At driver start time**, attach each target catalog with
  `--attach PATH=NAME` (repeatable). The driver runs
  `ATTACH IF NOT EXISTS 'PATH' AS NAME` once, on the same connection it uses
  for ticking. When a tick fires a job whose `database` is non-NULL, the
  driver `USE`s that catalog, runs the job's SQL, and restores the
  scheduler's own catalog — regardless of whether the job SQL succeeded.

If a job references a catalog that isn't attached, the driver marks
`last_status = 'skipped'`, records
`last_error = 'cron: database <name> is not attached'`, and moves on to the
next due job. Missing catalogs never crash a tick.

```bash
# 1. Set up the target catalog (the workload database).
ducklink -- app.duckdb -c 'CREATE TABLE app_events(t TIMESTAMP);'

# 2. Initialise cron in its own catalog.
ducklink cron init --db cron.duckdb

# 3. Schedule a job whose SQL runs against `app`, not `cron`.
ducklink cron schedule --db cron.duckdb \
    --database app \
    'log-heartbeat' '* * * * *' \
    "INSERT INTO app_events VALUES (now())"

# 4. Start the driver with app.duckdb attached as `app`.
ducklink cron run --db cron.duckdb --attach app.duckdb=app --interval 30
```

Notes:

- Only the native driver supports `--attach` in v1; `--wasm-driver` errors
  when combined with `--attach`.
- The catalog switch is per-job on the persistent connection, so jobs
  targeting different catalogs run sequentially, not in parallel.

## Running multiple drivers

Two shapes are supported: **safe coexistence** (multiple drivers point at
the same DB, only one ticks at a time) and **per-node targeting** (jobs
assigned to a specific driver).

### Whole-tick lease

Every driver identifies itself with a node name — from `--node`, else
`$DUCKLINK_CRON_NODE`, else `<hostname>-<pid>`. At the start of each tick the
driver acquires a row in `__cron_leases` under the key `tick`; if another
driver already holds a non-expired lease, this tick logs a line and skips:

```
cron: another driver holds the tick lease (holder=`node-b-1234`, expires in 42s); skipping
```

The lease TTL is `max(60s, 5 × --interval)`. A crashed holder's lease expires
naturally on the next tick after `expires_at`, and the next driver takes it
over. This is the whole scheduler's mutex — fine-grained per-job leases are
a v2 concern.

Two useful setups:

- **Hot-standby**: point two drivers at the same DB with the same interval.
  One "wins" each tick; the other logs a skip. If the winner crashes, the
  loser starts firing within TTL seconds.
- **Active-active with targeting**: give each driver a distinct `--node NAME`
  and use `--node` on `cron schedule` to route specific jobs to specific
  drivers. Anyone-goes jobs (no `--node` at schedule time) still race for
  the tick lease and fire once total per window.

### Per-node targeting

`cron schedule --node NodeA 'my-job' '...' '...'` sets `nodename = 'NodeA'` on
the job. That job only fires when a driver running as `--node NodeA` ticks —
other nodes skip it. Jobs scheduled without `--node` (the default; `nodename
IS NULL`) fire on any driver.

```bash
# Driver A only:
ducklink cron schedule --db app.duckdb --node NodeA 'a-only' '*/5 * * * *' 'CHECKPOINT'
# Any driver:
ducklink cron schedule --db app.duckdb 'shared' '*/5 * * * *' "DELETE FROM cache WHERE ts < now() - INTERVAL 1 HOUR"
# Fire jobs targeted at NodeA plus any shared jobs:
ducklink cron run --db app.duckdb --node NodeA
```

## Driver kinds

Two drivers today; both produce equivalent end-to-end behaviour.

- **Native (default).** A Rust tick loop inside `ducklink`. No extra artifact.
- **Wasm (`--wasm-driver`).** Dispatches to `cron-driver-tool.wasm`, a
  `wasi:cli/run` component. Portable to any wasi-preview2 host, and captures
  real per-job errors so `last_status` / `last_error` are accurate.

The wasm path is how you would run the same scheduler from a browser tab, a
Cloudflare Worker triggered by a cron rule, or another edge runtime. It imports
a small `duckdb:driver/exec` interface the embedding host implements against
its own DuckDB connection.

## Deployment recipes

### Foreground CLI

Run inside `tmux` / `screen`: `ducklink cron run --db /var/lib/app/app.duckdb --interval 30`.

### launchd (macOS)

`~/Library/LaunchAgents/ai.ducklink.cron.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>Label</key><string>ai.ducklink.cron</string>
  <key>ProgramArguments</key><array>
    <string>/usr/local/bin/ducklink</string>
    <string>cron</string><string>run</string>
    <string>--db</string><string>/Users/me/app.duckdb</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
```

Load with `launchctl load ~/Library/LaunchAgents/ai.ducklink.cron.plist`.

### systemd (Linux)

`/etc/systemd/system/ducklink-cron.service`:

```ini
[Unit]
Description=DuckLink cron driver
After=network.target

[Service]
ExecStart=/usr/local/bin/ducklink cron run --db /var/lib/ducklink/app.duckdb --interval 30
Restart=always
User=ducklink

[Install]
WantedBy=multi-user.target
```

`systemctl enable --now ducklink-cron.service`.

## Host-agnostic (browser / Worker / edge) hook

Embedders using the ducklink wasm core directly can drive cron with a JS
timer — no separate driver process. The whole scheduler is pure SQL over the
`cron_due(now_ms)` table macro, so the loop is trivial:

```javascript
// Assumes `db` is an open ducklink-wasm connection with LOAD cron;
// LOAD cron_scheduler; already run and the bootstrap DDL installed.
async function tick(db) {
  const now = Date.now();
  const due = await db.query(`SELECT id, sql FROM cron_due(${now})`);
  await db.exec(`UPDATE __cron_jobs
                 SET next_run_at = cron_advance(schedule, next_run_at, ${now}, catch_up),
                     last_run_at = ${now}
                 WHERE active AND next_run_at IS NOT NULL AND next_run_at <= ${now}`);
  for (const { id, sql } of due) await db.exec(sql);
}
setInterval(() => tick(db).catch(console.error), 30_000);
```

Behaviour matches the CLI driver — the CLI is just this loop in Rust.

## Job table reference

### `__cron_jobs`

| Column | Type | Meaning |
| --- | --- | --- |
| `id` | `UBIGINT` | `hash(name)` — stable, so re-scheduling by name upserts. |
| `name` | `TEXT` | Unique job name. |
| `schedule` | `TEXT` | 5-field cron expression (UTC). |
| `sql` | `TEXT` | Statement (or `;`-separated batch) to execute. |
| `active` | `BOOLEAN` | `activate` / `deactivate` toggle. |
| `catch_up` | `TEXT` | `skip` or `run_once`. |
| `next_run_at` | `BIGINT` | UTC epoch ms of the next fire. `NULL` = never. |
| `last_run_at` | `BIGINT` | UTC epoch ms of the last tick that fired this job. |
| `last_status` | `TEXT` | `fired` or `failed`. |
| `last_error` | `TEXT` | Last error message (or `NULL`). |
| `created_at` | `BIGINT` | UTC epoch ms of insertion. |
| `nodename` | `TEXT` | Per-node targeting: `NULL` = any driver; a value = fires only on the matching driver. |
| `database` | `TEXT` | Cross-catalog target: `NULL` = run in the current catalog; a value = `USE <value>` before the SQL. |

### `__cron_leases`

Whole-scheduler mutex (one row, key `'tick'`). Populated automatically by
`ducklink cron run`.

| Column | Type | Meaning |
| --- | --- | --- |
| `key` | `TEXT` | Always `'tick'` in v0 (fine-grained per-job leases are a future addition). |
| `holder` | `TEXT` | Node identity of the current holder. |
| `acquired_at` | `BIGINT` | UTC epoch ms of the acquire. |
| `expires_at` | `BIGINT` | UTC epoch ms after which any driver may steal. |

### `__cron_runs`

Append-only history, one row per fire. No auto-trim in v0 — callers prune.

| Column | Type | Meaning |
| --- | --- | --- |
| `job_id` | `UBIGINT` | FK to `__cron_jobs.id`. |
| `run_at` | `BIGINT` | UTC epoch ms of the fire. |
| `ok` | `BOOLEAN` | `true` if the job's SQL succeeded. |
| `err` | `TEXT` | Error message when `ok = false`. |
| `ms` | `BIGINT` | Wall-clock duration of the fire. |

## Known limitations / v1 roadmap

- **Whole-tick lease only.** The `__cron_leases` lock is scheduler-wide; a
  slow job on one driver blocks another driver from ticking until the TTL
  expires or the holder releases. Fine-grained per-job leases are a v2 add.
- **Node identity default is `<hostname>-<pid>`.** Restarted drivers get a
  new identity (new PID). Set `--node NAME` explicitly if you want a stable
  identity across restarts.
- **Cross-catalog jobs require `--attach PATH=NAME` on the driver command
  line;** the driver doesn't discover attached catalogs on its own.
- **The catalog switch uses `USE` on the persistent connection;** jobs
  targeting different catalogs run sequentially, not in parallel.
