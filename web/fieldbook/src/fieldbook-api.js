// Fieldbook data operations, expressed as direct SQL against the
// `__fieldbook_*` backing tables — no engine scalars, no nested-exec.
//
// Schema is byte-identical to `fieldbook-core::CREATE_BOOKS/CREATE_ENTRIES/
// CREATE_RUNS` and the fieldbook-dotcmd bootstrap (extensions/fieldbook-dotcmd/
// src/lib.rs::ensure_bootstrap), so a .duckdb file exported from here opens
// cleanly in the native `fieldbook` CLI, and vice versa. Doing this at the
// SQL layer rather than through the engine's scalars follows the dotcmd
// pattern — mutations land on the primary connection and are immediately
// visible to reads (no sibling-connection cache coherence problem).

// Bootstrap SQL — hard-coded here rather than fetched from the engine so
// this module has no runtime dependency on fieldbook.wasm. Kept in sync
// with fieldbook-core (see the shared constant strings there).
const DDL = [
  `CREATE TABLE IF NOT EXISTS __fieldbook_books (
     name        TEXT PRIMARY KEY,
     description TEXT,
     created_at  TIMESTAMP DEFAULT current_timestamp,
     updated_at  TIMESTAMP DEFAULT current_timestamp
   )`,
  `CREATE TABLE IF NOT EXISTS __fieldbook_entries (
     fieldbook   TEXT NOT NULL,
     ordinal     BIGINT NOT NULL,
     title       TEXT,
     source      TEXT NOT NULL,
     created_at  TIMESTAMP DEFAULT current_timestamp,
     updated_at  TIMESTAMP DEFAULT current_timestamp,
     PRIMARY KEY (fieldbook, ordinal)
   )`,
  `CREATE TABLE IF NOT EXISTS __fieldbook_runs (
     fieldbook    TEXT NOT NULL,
     ordinal      BIGINT NOT NULL,
     run_id       BIGINT NOT NULL,
     started_at   TIMESTAMP DEFAULT current_timestamp,
     duration_ms  BIGINT,
     status       TEXT,
     error        TEXT,
     row_count    BIGINT
   )`,
  `CREATE TABLE IF NOT EXISTS __fieldbook_state (
     key   TEXT PRIMARY KEY,
     value TEXT
   )`,
]

// Single-quote-escape a SQL text literal (identical helper to
// `extensions/fieldbook-dotcmd/src/lib.rs::sql_literal`).
export function sqlLiteral(s) {
  let out = "'"
  for (const c of String(s)) {
    if (c === "'") out += "'"
    out += c
  }
  out += "'"
  return out
}

export async function bootstrapSchema(db, conn) {
  for (const sql of DDL) {
    await db.execute(conn, sql)
  }
}

// Ensure the named fieldbook row exists. Idempotent: ON CONFLICT DO NOTHING.
export async function ensureFieldbook(db, conn, name) {
  await db.execute(
    conn,
    `INSERT INTO __fieldbook_books (name) VALUES (${sqlLiteral(name)}) ON CONFLICT DO NOTHING`,
  )
}

// Return `[{ordinal, source, title, updated_at}, ...]` for the named
// fieldbook, in ordinal order. Each row is a plain object with column names.
export async function listEntries(db, conn, name) {
  const result = await db.execute(
    conn,
    `SELECT ordinal, coalesce(title, '') AS title, source, updated_at
       FROM __fieldbook_entries
      WHERE fieldbook = ${sqlLiteral(name)}
      ORDER BY ordinal`,
  )
  return rowsToObjects(result)
}

// Append a new entry, allocating the next ordinal for this fieldbook.
// Returns the assigned ordinal.
export async function addEntry(db, conn, name, source) {
  const nextRes = await db.execute(
    conn,
    `SELECT COALESCE(MAX(ordinal), 0) + 1
       FROM __fieldbook_entries
      WHERE fieldbook = ${sqlLiteral(name)}`,
  )
  const rows = rowsToObjects(nextRes)
  const nextOrd = Number(
    rows[0] ? Object.values(rows[0])[0] : 1n,
  )
  await db.execute(
    conn,
    `INSERT INTO __fieldbook_entries (fieldbook, ordinal, source)
     VALUES (${sqlLiteral(name)}, ${nextOrd}, ${sqlLiteral(source)})`,
  )
  await db.execute(
    conn,
    `UPDATE __fieldbook_books
        SET updated_at = current_timestamp
      WHERE name = ${sqlLiteral(name)}`,
  )
  return nextOrd
}

// Update an entry's source text in place (SQL edits inside a cell).
export async function updateEntry(db, conn, name, ordinal, source) {
  await db.execute(
    conn,
    `UPDATE __fieldbook_entries
        SET source = ${sqlLiteral(source)},
            updated_at = current_timestamp
      WHERE fieldbook = ${sqlLiteral(name)}
        AND ordinal   = ${ordinal}`,
  )
}

// Delete an entry (and any recorded runs for it). Ordinals are NOT
// renumbered — matches the native CLI's semantics; the notebook UI
// displays whatever ordinals are present.
export async function deleteEntry(db, conn, name, ordinal) {
  await db.execute(
    conn,
    `DELETE FROM __fieldbook_runs
      WHERE fieldbook = ${sqlLiteral(name)} AND ordinal = ${ordinal}`,
  )
  await db.execute(
    conn,
    `DELETE FROM __fieldbook_entries
      WHERE fieldbook = ${sqlLiteral(name)} AND ordinal = ${ordinal}`,
  )
}

// Execute a user cell's SQL and return `{ result, durationMs, error }`.
// Never throws — reports errors as strings on the returned object so the
// cell UI can render them inline.
export async function runCellSql(db, conn, sql) {
  const started = performance.now()
  try {
    const result = await db.execute(conn, sql)
    const durationMs = Math.round(performance.now() - started)
    return { result, durationMs, error: null }
  } catch (e) {
    const durationMs = Math.round(performance.now() - started)
    const msg = (e && (e.payload || e.message)) || String(e)
    return { result: null, durationMs, error: msg }
  }
}

// Record a run outcome into `__fieldbook_runs`. Best-effort — a record
// failure is swallowed so it doesn't mask the actual query outcome.
export async function recordRun(db, conn, name, ordinal, runId, durationMs, status, error, rowCount) {
  try {
    const errSql = error ? sqlLiteral(error) : 'NULL'
    const rowsSql = rowCount != null && rowCount >= 0 ? String(rowCount) : 'NULL'
    await db.execute(
      conn,
      `INSERT INTO __fieldbook_runs
         (fieldbook, ordinal, run_id, duration_ms, status, error, row_count)
       VALUES (${sqlLiteral(name)}, ${ordinal}, ${runId}, ${durationMs},
               ${sqlLiteral(status)}, ${errSql}, ${rowsSql})`,
    )
  } catch { /* best-effort */ }
}

// Convert the DuckDB execute() shape (`{columns:[{name,type}], rows:[[v...]]}`)
// into row-objects keyed by column name.
export function rowsToObjects(result) {
  if (!result || !result.rows || !result.columns) return []
  const names = result.columns.map((c) => c.name)
  return result.rows.map((r) => {
    const o = {}
    for (let i = 0; i < names.length; i++) o[names[i]] = r[i]
    return o
  })
}

// Increment-and-fetch a fresh run_id — one per "run all" invocation.
export async function nextRunId(db, conn) {
  const r = await db.execute(
    conn,
    `SELECT COALESCE(MAX(run_id), 0) + 1 FROM __fieldbook_runs`,
  )
  const rows = rowsToObjects(r)
  return Number(rows[0] ? Object.values(rows[0])[0] : 1n)
}
