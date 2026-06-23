// Browser entry: load the sample extension into the in-browser DuckDB and call
// one of its registered functions — the full loader pipeline (native host's job)
// running in the browser.
import { instantiateCore } from './run-core.mjs'
import { createExtensionHost } from './extension-host.mjs'

async function bytes(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(`fetch ${url}: ${r.status}`)
  return new Uint8Array(await r.arrayBuffer())
}

async function main() {
  const out = document.getElementById('out')
  out.dataset.status = 'running'
  try {
    const [coreBytes, extBytes] = await Promise.all([
      bytes('./ducklink_core.wasm'),
      bytes('./sample_extension.wasm'),
    ])

    const host = createExtensionHost()
    await host.preload('sample_extension', extBytes)

    const db = await instantiateCore(coreBytes, host.coreImports())
    const conn = db.open(undefined)
    await db.execute(conn, 'LOAD sample_extension')

    // Exercise every capability the sample extension registers — scalar / table
    // / aggregate / cast dispatch back to the loaded extension instance, while
    // macro and logical-type run as core SQL — all in the browser.
    const cases = [
      ['scalar      sample_plus_one(41)', 'SELECT sample_plus_one(41) AS v'],
      ['macro       sample_add_two(40)', 'SELECT sample_add_two(40) AS v'],
      ['cast        id-7 -> sample_id', "SELECT cast('id-7' AS sample_id) AS v"],
      ['logical     7::sample_id', 'SELECT 7::sample_id AS v'],
      ['table       sample_emit_sequence(4)', 'SELECT * FROM sample_emit_sequence(4)'],
      ['aggregate   sample_sum(1..4)', 'SELECT sample_sum(v) AS v FROM (VALUES (1),(2),(3),(4)) AS t(v)'],
      ["replacement FROM 'hello.sample'", "SELECT * FROM 'hello.sample'"],
    ]
    let failed = 0
    // BigInt-safe stringify: typed integer columns come back as JS BigInt.
    const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? `${v}n` : v))
    // execute is JSPI-promised (async); run cases sequentially so the await
    // chain preserves order and the shared connection's statement ordering.
    const lines = []
    for (const [label, sql] of cases) {
      try {
        const result = await db.execute(conn, sql)
        lines.push(label.padEnd(38) + ' = ' + ser(result.rows))
      } catch (e) {
        failed++
        lines.push(label.padEnd(38) + ' = ERROR ' + ser((e && e.payload) || String(e)))
      }
    }
    db.close(conn)

    out.textContent = lines.join('\n')
    out.dataset.status = failed === 0 ? 'ok' : 'error'
  } catch (e) {
    out.textContent = 'ERROR: ' + (e && (e.stack || e.message) || e)
    out.dataset.status = 'error'
  }
}

main()
