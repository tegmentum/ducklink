// Browser entry: exercise the core's prepared-statement API directly (no host),
// proving prepare + positional bind + repeated execution work in the browser.
import { instantiateCore } from './run-core.mjs'
import { tableFromIPC } from 'apache-arrow'

async function bytes(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(`fetch ${url}: ${r.status}`)
  return new Uint8Array(await r.arrayBuffer())
}

async function main() {
  const out = document.getElementById('out')
  out.dataset.status = 'running'
  try {
    const coreBytes = await bytes('./ducklink_core.wasm')
    const db = await instantiateCore(coreBytes)
    const conn = db.open(undefined)

    const lines = []
    let failed = 0
    // BigInt-safe serialization (jco maps int64 to BigInt).
    const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? `${v}n` : v))
    const check = (label, got, want) => {
      const ok = ser(got) === ser(want)
      if (!ok) failed++
      lines.push((ok ? 'ok   ' : 'FAIL ') + label.padEnd(34) + ' = ' + ser(got))
    }

    // Positional parameters, reused across executions with different bindings.
    // Results come back typed: a BIGINT column is int64 (a BigInt in JS).
    const stmt = db.prepare(conn, 'SELECT CAST($1 AS BIGINT) + CAST($2 AS BIGINT) AS total')
    check('parameter-count', stmt.parameterCount(), 2)
    const a = stmt.execute([{ tag: 'int64', val: 40n }, { tag: 'int64', val: 2n }])
    check('execute(40, 2)', a.rows, [[{ tag: 'int64', val: 42n }]])
    const b = stmt.execute([{ tag: 'int64', val: 100n }, { tag: 'int64', val: 1n }])
    check('reuse execute(100, 1)', b.rows, [[{ tag: 'int64', val: 101n }]])

    // Mixed types: a TEXT column and a BOOLEAN column (typed, not stringified).
    const stmt2 = db.prepare(conn, 'SELECT $1 AS label, $2 IS NULL AS is_null')
    const c = stmt2.execute([{ tag: 'text', val: 'hi' }, { tag: 'null' }])
    check('text + null', c.rows, [[{ tag: 'text', val: 'hi' }, { tag: 'boolean', val: true }]])

    // Config API: open a connection with options applied.
    const cfgConn = db.openWithConfig(undefined, [['default_order', 'desc']])
    const cfg = db.execute(cfgConn, "SELECT current_setting('default_order') AS v")
    check('open-with-config', cfg.rows, [[{ tag: 'text', val: 'DESC' }]])
    db.close(cfgConn)

    // Arrow IPC: decode the bytes with apache-arrow and check the values.
    const arrow = db.queryArrow(conn, 'SELECT i::INTEGER AS n FROM range(3) t(i)')
    const table = tableFromIPC(arrow)
    const decoded = Array.from(table.getChild('n').toArray(), Number)
    check('query-arrow (decoded)', decoded, [0, 1, 2])

    db.close(conn)
    out.textContent = lines.join('\n')
    out.dataset.status = failed === 0 ? 'ok' : 'error'
  } catch (e) {
    out.textContent = 'ERROR: ' + ((e && (e.stack || e.message)) || e)
    out.dataset.status = 'error'
  }
}

main()
