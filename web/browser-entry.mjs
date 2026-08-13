// Browser entry: fetch the DuckLink core wasip2 component, boot it through
// the wasm-cm runtime-guest driver (@tegmentum/wasmos-browser +
// @wasmos/runtime-guest-bridge), run a query, and paint the result into the
// page. Vite serves this as native ESM. See `run-core.mjs` for the boot flow;
// see `README.md` for the wider architecture.
import { runQuery } from './run-core.mjs'

async function main() {
  const out = document.getElementById('out')
  out.dataset.status = 'running'
  try {
    const resp = await fetch('./ducklink_core.wasm')
    const bytes = new Uint8Array(await resp.arrayBuffer())
    const result = await runQuery(bytes, 'SELECT 42 AS answer, 1 + 1 AS two')
    // Typed integer columns come back as JS BigInt inside the WIT `duckvalue`
    // variant; serialize BigInts safely so the page renders them as text.
    const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? `${v}n` : v))
    out.textContent =
      'columns: ' + result.columns.map((c) => c.name).join(', ') + '\n' +
      'rows: ' + ser(result.rows)
    out.dataset.status = 'ok'
  } catch (e) {
    out.textContent = 'ERROR: ' + (e && (e.stack || e.message) || e)
    out.dataset.status = 'error'
  }
}

main()
