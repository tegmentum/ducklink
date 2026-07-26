// Browser entry for the cron demo: load ducklink_core + cron + cron_scheduler
// into the in-browser DuckDB, seed a job, and drive a tick loop from
// setInterval. This is the "browser tab is the driver" shape the wasm-driver
// design pitched; here JavaScript is the wasi-preview2 host, so we run the
// same SQL the native `ducklink cron run` tick loop runs (see cron_cli::tick).
import { instantiateCore } from './run-core.mjs'
import { createExtensionHost } from './extension-host.mjs'

async function bytes(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(`fetch ${url}: ${r.status}`)
  return new Uint8Array(await r.arrayBuffer())
}

// Match extensions/cron_scheduler-component/src/lib.rs::BOOTSTRAP_SQL. Kept
// inline (rather than fetched from SELECT cron_bootstrap_sql()) so the schema
// is visible in the demo source itself. Same additive shape the CLI uses.
const BOOTSTRAP_SQL = `
CREATE TABLE IF NOT EXISTS __cron_jobs (
    id UBIGINT PRIMARY KEY, name TEXT NOT NULL UNIQUE, schedule TEXT NOT NULL,
    sql TEXT NOT NULL, active BOOLEAN NOT NULL DEFAULT true,
    catch_up TEXT NOT NULL DEFAULT 'skip', next_run_at BIGINT, last_run_at BIGINT,
    last_status TEXT, last_error TEXT,
    created_at BIGINT NOT NULL DEFAULT (epoch_ms(now())),
    nodename TEXT, database TEXT
);
CREATE TABLE IF NOT EXISTS __cron_runs (
    job_id UBIGINT NOT NULL, run_at BIGINT NOT NULL, ok BOOLEAN NOT NULL,
    err TEXT, ms BIGINT
);
CREATE TABLE IF NOT EXISTS __cron_leases (
    key TEXT NOT NULL PRIMARY KEY, holder TEXT NOT NULL,
    acquired_at BIGINT NOT NULL, expires_at BIGINT NOT NULL
);
CREATE OR REPLACE MACRO cron_id(name) AS hash(name);
CREATE OR REPLACE MACRO cron_due(now_ms) AS TABLE
    SELECT id, name, schedule, sql, catch_up, next_run_at, last_run_at, nodename, database
    FROM __cron_jobs
    WHERE active AND next_run_at IS NOT NULL AND next_run_at <= now_ms AND nodename IS NULL
    ORDER BY next_run_at;
`

const NODE = 'browser-tab'
const INTERVAL_MS = 3_000
const LEASE_TTL_MS = 60_000

// BigInt-safe stringify — the wasm boundary hands typed ints back as BigInt.
const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? Number(v) : v), 2)

async function main() {
  const status = document.getElementById('status')
  const jobsBox = document.getElementById('jobs')
  const logBox = document.getElementById('log')
  const log = (msg, kind) => {
    const line = document.createElement('div')
    if (kind) line.className = kind
    line.textContent = `[${new Date().toISOString().slice(11, 19)}] ${msg}`
    logBox.prepend(line)
  }
  status.dataset.status = 'running'
  status.textContent = 'loading wasm…'

  const [coreBytes, cronBytes, cronSchedBytes] = await Promise.all([
    bytes('./ducklink_core.wasm'),
    bytes('./cron.wasm'),
    bytes('./cron_scheduler.wasm'),
  ])

  const host = createExtensionHost()
  await host.preload('cron', cronBytes)
  await host.preload('cron_scheduler', cronSchedBytes)

  const db = await instantiateCore(coreBytes, host.coreImports())
  const conn = db.open(undefined)
  await db.execute(conn, 'LOAD cron')
  await db.execute(conn, 'LOAD cron_scheduler')
  for (const stmt of BOOTSTRAP_SQL.split(';').map((s) => s.trim()).filter(Boolean)) {
    await db.execute(conn, stmt)
  }
  status.textContent = 'ready — press a button.'

  async function refreshJobs() {
    const result = await db.execute(
      conn,
      'SELECT name, schedule, next_run_at, last_run_at, last_status FROM __cron_jobs ORDER BY name'
    )
    jobsBox.textContent = ser(result.rows)
  }

  // The tick body: read due jobs, pre-advance next_run_at, fire each one,
  // record __cron_runs, release the per-job lease. Identical shape to
  // cron_cli::tick_body in Rust — this file is the counterpart proof that
  // "the whole scheduler is pure SQL" holds outside the CLI.
  async function tick() {
    const now = Date.now()
    // Per-job lease acquire happens inside the loop below; a global lease
    // for the whole scheduler is v0 semantics we deliberately do not
    // reproduce here (see cron.md "Per-job leases").
    const dueRes = await db.execute(
      conn,
      `SELECT id, sql FROM cron_due(${now})`
    )
    if (dueRes.rows.length === 0) return { fired: 0, contended: 0 }

    // Pre-advance every due row BEFORE firing (skip-semantics: a crash
    // mid-tick doesn't re-fire the same window).
    await db.execute(
      conn,
      `UPDATE __cron_jobs SET next_run_at = cron_advance(schedule, next_run_at, ${now}, catch_up),
             last_run_at = ${now}
         WHERE active AND next_run_at IS NOT NULL AND next_run_at <= ${now}
           AND nodename IS NULL`
    )

    let fired = 0
    let contended = 0
    for (const row of dueRes.rows) {
      const id = row[0]
      const sql = row[1]
      const leaseKey = `job:${id}`
      const acquire = await db.execute(
        conn,
        `INSERT INTO __cron_leases (key, holder, acquired_at, expires_at)
         VALUES ('${leaseKey}', '${NODE}', ${now}, ${now + LEASE_TTL_MS})
         ON CONFLICT (key) DO UPDATE SET
             holder = excluded.holder,
             acquired_at = excluded.acquired_at,
             expires_at = excluded.expires_at
         WHERE __cron_leases.expires_at <= ${now}
         RETURNING holder`
      )
      const heldByUs = acquire.rows[0] && acquire.rows[0][0] === NODE
      if (!heldByUs) {
        contended++
        continue
      }
      const t0 = performance.now()
      let ok = true
      let errMsg = null
      try {
        await db.execute(conn, sql)
      } catch (e) {
        ok = false
        errMsg = (e && e.payload && e.payload.message) || (e && e.message) || String(e)
      }
      const ms = Math.round(performance.now() - t0)
      const status = ok ? 'fired' : 'failed'
      const errLit = errMsg ? `'${errMsg.replace(/'/g, "''")}'` : 'NULL'
      await db.execute(
        conn,
        `UPDATE __cron_jobs SET last_status = '${status}', last_error = ${errLit} WHERE id = ${id};`
      )
      await db.execute(
        conn,
        `INSERT INTO __cron_runs VALUES (${id}, ${now}, ${ok}, ${errLit}, ${ms})`
      )
      await db.execute(
        conn,
        `UPDATE __cron_leases SET expires_at = 0 WHERE key = '${leaseKey}' AND holder = '${NODE}'`
      )
      fired++
    }
    return { fired, contended }
  }

  async function runTick(reason) {
    try {
      const { fired, contended } = await tick()
      if (fired > 0 || contended > 0) {
        log(
          `${reason}: fired ${fired}, contended ${contended}`,
          fired > 0 ? 'ok' : undefined
        )
        await refreshJobs()
      } else {
        log(`${reason}: nothing due`)
      }
    } catch (e) {
      log(`${reason} error: ${e && e.message ? e.message : e}`, 'err')
    }
  }

  document.getElementById('schedule').addEventListener('click', async () => {
    // Schedule (or re-schedule) a demo job that fires every minute. Any
    // valid DuckDB SQL works here — SELECT now() is safe and observable.
    try {
      await db.execute(
        conn,
        `INSERT INTO __cron_jobs (id, name, schedule, sql, active, catch_up, next_run_at, nodename, database)
         VALUES (cron_id('demo'), 'demo', '* * * * *', 'SELECT now();', true, 'skip',
                 cron_next('* * * * *', epoch_ms(now())), NULL, NULL)
         ON CONFLICT (id) DO UPDATE SET
             schedule = excluded.schedule, sql = excluded.sql,
             active = true, next_run_at = cron_next(excluded.schedule, epoch_ms(now()))`
      )
      log('scheduled demo job (`* * * * *` → SELECT now())', 'ok')
      await refreshJobs()
    } catch (e) {
      log(`schedule error: ${e && e.message ? e.message : e}`, 'err')
    }
  })
  document.getElementById('tick').addEventListener('click', () => runTick('manual tick'))
  document.getElementById('stop').addEventListener('click', () => {
    if (autoTick) {
      clearInterval(autoTick)
      autoTick = null
      log('stopped auto-tick')
    }
  })

  await refreshJobs()
  let autoTick = setInterval(() => runTick('auto tick'), INTERVAL_MS)
  log(`auto-ticking every ${INTERVAL_MS / 1000}s — press Stop to halt`)
}

main().catch((e) => {
  const s = document.getElementById('status')
  s.dataset.status = 'error'
  s.textContent = 'FATAL: ' + (e && (e.stack || e.message) || e)
})
