// Browser entry: exercise the core's prepared-statement API directly (no host),
// proving prepare + positional bind + repeated execution work in the browser
// on the wasm-cm runtime-guest lane.
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
    const openRes = await db.open(undefined)
    if (openRes.tag === 'err') throw new Error(`open: ${openRes.val}`)
    const conn = openRes.val

    const lines = []
    let failed = 0
    const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? `${v}n` : v))
    const check = (label, got, want) => {
      const ok = ser(got) === ser(want)
      if (!ok) failed++
      lines.push((ok ? 'ok   ' : 'FAIL ') + label.padEnd(34) + ' = ' + ser(got))
    }

    // Positional parameters, reused across executions with different bindings.
    const prepRes = await db.prepare(conn, 'SELECT CAST($1 AS BIGINT) + CAST($2 AS BIGINT) AS total')
    if (prepRes.tag === 'err') throw new Error(`prepare: ${JSON.stringify(prepRes.val)}`)
    const stmt = prepRes.val
    check('parameter-count', await stmt.parameterCount(), 2)
    const a = await stmt.execute([{ tag: 'int64', val: 40n }, { tag: 'int64', val: 2n }])
    if (a.tag === 'err') throw new Error(`execute(40,2): ${JSON.stringify(a.val)}`)
    check('execute(40, 2)', a.val.rows, [[{ tag: 'int64', val: 42n }]])
    const b = await stmt.execute([{ tag: 'int64', val: 100n }, { tag: 'int64', val: 1n }])
    if (b.tag === 'err') throw new Error(`execute(100,1): ${JSON.stringify(b.val)}`)
    check('reuse execute(100, 1)', b.val.rows, [[{ tag: 'int64', val: 101n }]])

    // Mixed types.
    const prep2 = await db.prepare(conn, 'SELECT $1 AS label, $2 IS NULL AS is_null')
    if (prep2.tag === 'err') throw new Error(`prepare2: ${JSON.stringify(prep2.val)}`)
    const stmt2 = prep2.val
    const c = await stmt2.execute([{ tag: 'text', val: 'hi' }, { tag: 'null' }])
    if (c.tag === 'err') throw new Error(`execute text+null: ${JSON.stringify(c.val)}`)
    check('text + null', c.val.rows, [[{ tag: 'text', val: 'hi' }, { tag: 'boolean', val: true }]])

    // Config API: open a connection with options applied.
    const cfgOpen = await db.openWithConfig(undefined, [['default_order', 'desc']])
    if (cfgOpen.tag === 'err') throw new Error(`openWithConfig: ${cfgOpen.val}`)
    const cfgConn = cfgOpen.val
    const cfg = await db.execute(cfgConn, "SELECT current_setting('default_order') AS v")
    if (cfg.tag === 'err') throw new Error(`current_setting: ${JSON.stringify(cfg.val)}`)
    check('open-with-config', cfg.val.rows, [[{ tag: 'text', val: 'DESC' }]])
    await db.close(cfgConn)

    // Arrow IPC: decode the bytes with apache-arrow and check the values.
    // `queryArrow` returns `list<u8>` from the runtime-guest binding, which the
    // decoder hands back as a plain array of numbers rather than a typed
    // Uint8Array — `tableFromIPC` needs the typed view.
    const arrowRes = await db.queryArrow(conn, 'SELECT i::INTEGER AS n FROM range(3) t(i)')
    if (arrowRes.tag === 'err') throw new Error(`queryArrow: ${JSON.stringify(arrowRes.val)}`)
    const table = tableFromIPC(Uint8Array.from(arrowRes.val))
    const decoded = Array.from(table.getChild('n').toArray(), Number)
    check('query-arrow (decoded)', decoded, [0, 1, 2])

    await db.close(conn)
    out.textContent = lines.join('\n')
    out.dataset.status = failed === 0 ? 'ok' : 'error'
  } catch (e) {
    out.textContent = 'ERROR: ' + ((e && (e.stack || e.message)) || e)
    out.dataset.status = 'error'
  }
}

main()
