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
      bytes('./duckdb_core_component.wasm'),
      bytes('./sample_extension.wasm'),
    ])

    const host = createExtensionHost()
    await host.preload('sample_extension', extBytes)

    const db = await instantiateCore(coreBytes, host.coreImports())
    const conn = db.open(undefined)
    db.execute(conn, 'LOAD sample_extension')
    const scalar = db.execute(conn, 'SELECT sample_plus_one(41) AS scalar')
    const macro = db.execute(conn, 'SELECT sample_add_two(40) AS macro')
    const cast = db.execute(conn, "SELECT cast('id-7' AS sample_id) AS cast")
    db.close(conn)

    out.textContent =
      'scalar sample_plus_one(41) = ' + JSON.stringify(scalar.rows) + '\n' +
      'macro  sample_add_two(40)  = ' + JSON.stringify(macro.rows) + '\n' +
      'cast   id-7 -> sample_id   = ' + JSON.stringify(cast.rows)
    out.dataset.status = 'ok'
  } catch (e) {
    out.textContent = 'ERROR: ' + (e && (e.stack || e.message) || e)
    out.dataset.status = 'error'
  }
}

main()
