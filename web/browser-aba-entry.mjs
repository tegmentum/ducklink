// Browser entry: load the real `aba` extension (@2.2.0, reconciled) into the
// in-browser DuckDB core and dispatch its scalar `aba_validate` through jco +
// extension-host.mjs — the proof that the rebuilt browser core composes with a
// real reconciled extension in Chromium.
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
      bytes('./aba.wasm'),
    ])

    const host = createExtensionHost()
    await host.preload('aba', extBytes)

    const db = await instantiateCore(coreBytes, host.coreImports())
    const conn = db.open(undefined)
    await db.execute(conn, 'LOAD aba')

    const cases = [
      ["aba_validate('021000021') (Chase)", "SELECT aba_validate('021000021') AS v"],
      ["aba_validate('121000248') (Wells)", "SELECT aba_validate('121000248') AS v"],
      ["aba_validate('021000020') (bad)", "SELECT aba_validate('021000020') AS v"],
      ["aba_validate('12345') (short)", "SELECT aba_validate('12345') AS v"],
    ]
    const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? `${v}n` : v))
    const lines = []
    let failed = 0
    for (const [label, sql] of cases) {
      try {
        const result = await db.execute(conn, sql)
        lines.push(label.padEnd(36) + ' = ' + ser(result.rows))
      } catch (e) {
        failed++
        lines.push(label.padEnd(36) + ' = ERROR ' + ser((e && e.payload) || String(e)))
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
