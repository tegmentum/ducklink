// Scenario-3 corpus harness: run every wasm extension's smoke.sql through the
// IN-BROWSER DuckDB and diff the `.mode csv`-shaped output against
// smoke.expected. Mirrors native-extension/ducklink/tests/scenario1_corpus.rs,
// but in the browser, on the wasm-cm runtime-guest lane.
//
// The in-browser extension host (extension-host.mjs) is single-extension per
// dispatch fan-out (callback handles are unique but registrations drain once),
// and 100+ extensions would collide on function names in one DuckDB catalog.
// So we re-instantiate the core + host FRESH per extension. Slow (each
// instantiation reparses the ~44 MB core), so the driver runs one extension
// per navigation via `?ext=<name>` — see `corpus-verify.mjs`.
import { createDuckLinkDriver, instantiateCore } from './run-core.mjs'
import { createExtensionHost } from './extension-host.mjs'

async function bytes(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(`fetch ${url}: ${r.status}`)
  return new Uint8Array(await r.arrayBuffer())
}

// --- smoke.sql / smoke.expected / CSV logic (ported from scenario1_corpus.rs) --

/** Split smoke.sql into statements, dropping --/.-comments + blanks, respecting
 *  single-quoted strings (with '' escape) so a ';' inside a literal is kept. */
function statements(sql) {
  const body = sql
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith('--') && !l.startsWith('.'))
    .join('\n')
  const out = []
  let cur = ''
  let inStr = false
  for (let i = 0; i < body.length; i++) {
    const c = body[i]
    if (c === "'" && inStr && body[i + 1] === "'") {
      cur += "''"
      i++
    } else if (c === "'") {
      inStr = !inStr
      cur += c
    } else if (c === ';' && !inStr) {
      const s = cur.trim()
      if (s) out.push(s)
      cur = ''
    } else {
      cur += c
    }
  }
  const tail = cur.trim()
  if (tail) out.push(tail)
  return out
}

/** Quote a CSV field like DuckDB's CLI `.mode csv`. */
function csvField(s) {
  return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s
}

/** Render a duckvalue cell ({tag,val}) like DuckDB's CLI `.mode csv`. */
function fmtCell(cell) {
  if (cell == null) return ''
  const tag = cell.tag
  const val = cell.val
  switch (tag) {
    case 'null':
    case 'none':
      return 'NULL'
    case 'boolean':
      return val ? 'true' : 'false'
    case 'int64':
    case 'uint64':
    case 'float64':
      return String(val)
    case 'text':
      return String(val)
    case 'blob': {
      const bytes = Uint8Array.from(val)
      let hex = '0x'
      for (const b of bytes) hex += b.toString(16).padStart(2, '0')
      return hex
    }
    default:
      if (typeof val === 'bigint') return String(val)
      if (cell.tag === undefined && typeof cell !== 'object') return String(cell)
      return val === undefined ? '' : String(val)
  }
}

/** Execute one statement -> CSV lines (header of column names, then rows). */
async function runStmt(db, conn, sql) {
  const res = await db.execute(conn, sql)
  if (res.tag === 'err') throw new Error(`${sql.slice(0, 80)}: ${JSON.stringify(res.val)}`)
  const result = res.val
  const header = (result.columns || []).map((c) => csvField(c.name)).join(',')
  const lines = [header]
  for (const row of result.rows || []) {
    lines.push(row.map((cell) => csvField(fmtCell(cell))).join(','))
  }
  return lines.flatMap((l) => l.split('\n'))
}

function normalize(lines) {
  return lines.map((l) => l.replace(/[\r ]+$/, '')).filter((l) => l !== '')
}

function expectedLines(text) {
  return normalize(text.split('\n').filter((l) => !l.trimStart().startsWith('#')))
}

function compare(produced, expected) {
  for (let i = 0; i < expected.length; i++) {
    const exp = expected[i]
    if (exp === '~~') continue
    const got = (produced[i] ?? '').replace(/[\r ]+$/, '')
    if (exp === '?') {
      if (got === '') return `line ${i}: expected non-empty, got empty`
      continue
    }
    if (got !== exp) return `line ${i}: expected ${JSON.stringify(exp)}, got ${JSON.stringify(got)}`
  }
  if (produced.length > expected.length) {
    return `produced ${produced.length} lines, expected ${expected.length}`
  }
  return null
}

// --- per-extension run ------------------------------------------------------

async function runExtension(coreBytes, entry) {
  const driverBundle = await createDuckLinkDriver({ jspi: 'auto' })
  const host = createExtensionHost(driverBundle)
  await host.preload(entry.name, await bytes(entry.wasmUrl))
  const db = await instantiateCore(coreBytes, host.coreImports(), { driverBundle })
  const openRes = await db.open(undefined)
  if (openRes.tag === 'err') throw new Error(`open: ${openRes.val}`)
  const conn = openRes.val
  try {
    const load = await db.execute(conn, `LOAD ${entry.name}`)
    if (load.tag === 'err') throw new Error(`LOAD ${entry.name}: ${JSON.stringify(load.val)}`)
    const produced = []
    for (const stmt of statements(entry.smokeSql)) {
      produced.push(...(await runStmt(db, conn, stmt)))
    }
    if (entry.smokeExpected == null) {
      return { name: entry.name, status: 'PASS', note: 'ran (no expected)' }
    }
    const diff = compare(normalize(produced), expectedLines(entry.smokeExpected))
    return diff
      ? { name: entry.name, status: 'MISMATCH', note: diff }
      : { name: entry.name, status: 'PASS' }
  } catch (e) {
    const msg = (e && e.message) || String(e)
    return { name: entry.name, status: 'ERROR', note: String(msg).slice(0, 300) }
  } finally {
    try { await db.close(conn) } catch {}
  }
}

async function main() {
  const out = document.getElementById('out')
  out.dataset.status = 'running'
  try {
    const name = new URLSearchParams(location.search).get('ext')
    const manifest = await (await fetch('./corpus-manifest.json')).json()
    const entry = manifest.find((m) => m.name === name)
    if (!entry) throw new Error(`unknown ext '${name}'`)
    const coreBytes = await bytes('./ducklink_core.wasm')
    const r = await runExtension(coreBytes, entry)
    out.textContent = JSON.stringify(r)
    out.dataset.status = 'ok'
  } catch (e) {
    out.textContent = JSON.stringify({ status: 'ERROR', note: String(e?.stack || e).slice(0, 300) })
    out.dataset.status = 'error'
  }
}

main()
