// Browser entry: load the sample extension into the in-browser DuckDB and
// call one of its registered functions — the full loader pipeline (native
// host's job) running in the browser, on the wasm-cm runtime-guest lane.
//
// Depends on `web/public/sample_extension.wasm` — dropped there by
// `copy-wasm.sh`. Regenerate the extension's runtime-guest bindings alongside
// the core's with `npm run generate` (which iterates every staged extension
// wasm) before running.
import { createDuckLinkDriver, instantiateCore } from './run-core.mjs'
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

    // Shared driver + polyfill so core + extension ride the same runtime-guest
    // and callback dispatch is a direct `driver.callExport` (no cross-runtime
    // hop). See run-core.mjs::createDuckLinkDriver.
    const driverBundle = await createDuckLinkDriver({ jspi: 'auto' })
    const host = createExtensionHost(driverBundle)
    await host.preload('sample_extension', extBytes)

    const db = await instantiateCore(coreBytes, host.coreImports(), { driverBundle })
    const openRes = await db.open(undefined)
    if (openRes.tag === 'err') throw new Error(`open: ${openRes.val}`)
    const conn = openRes.val
    const loadRes = await db.execute(conn, 'LOAD sample_extension')
    if (loadRes.tag === 'err') throw new Error(`LOAD sample_extension: ${JSON.stringify(loadRes.val)}`)

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
    const lines = []
    for (const [label, sql] of cases) {
      try {
        const res = await db.execute(conn, sql)
        if (res.tag === 'err') {
          failed++
          lines.push(label.padEnd(38) + ' = ERROR ' + ser(res.val))
        } else {
          lines.push(label.padEnd(38) + ' = ' + ser(res.val.rows))
        }
      } catch (e) {
        failed++
        lines.push(label.padEnd(38) + ' = ERROR ' + ser((e && e.message) || String(e)))
      }
    }
    await db.close(conn)

    out.textContent = lines.join('\n')
    out.dataset.status = failed === 0 ? 'ok' : 'error'
  } catch (e) {
    out.textContent = 'ERROR: ' + (e && (e.stack || e.message) || e)
    out.dataset.status = 'error'
  }
}

main()
