// Browser entry: load the real `aba` extension (reconciled) into the
// in-browser DuckDB core and dispatch its scalar `aba_validate` through the
// runtime-guest driver + extension-host.mjs — the proof that the rebuilt
// browser core composes with a real reconciled extension in Chromium.
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
      bytes('./aba.wasm'),
    ])

    const driverBundle = await createDuckLinkDriver({ jspi: 'auto' })
    const host = createExtensionHost(driverBundle)
    await host.preload('aba', extBytes)

    const db = await instantiateCore(coreBytes, host.coreImports(), { driverBundle })
    const openRes = await db.open(undefined)
    if (openRes.tag === 'err') throw new Error(`open: ${openRes.val}`)
    const conn = openRes.val
    const loadRes = await db.execute(conn, 'LOAD aba')
    if (loadRes.tag === 'err') throw new Error(`LOAD aba: ${JSON.stringify(loadRes.val)}`)

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
        const res = await db.execute(conn, sql)
        if (res.tag === 'err') {
          failed++
          lines.push(label.padEnd(36) + ' = ERROR ' + ser(res.val))
        } else {
          lines.push(label.padEnd(36) + ' = ' + ser(res.val.rows))
        }
      } catch (e) {
        failed++
        lines.push(label.padEnd(36) + ' = ERROR ' + ser((e && e.message) || String(e)))
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
