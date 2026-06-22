// Browser entry: fetch the DuckDB core component, run a query via wasi-polyfill,
// and write the result into the page. Bundle with esbuild (see README.md).
import { runQuery } from './run-core.mjs'

async function main() {
  const out = document.getElementById('out')
  out.dataset.status = 'running'
  try {
    const resp = await fetch('./ducklink_core.wasm')
    const bytes = new Uint8Array(await resp.arrayBuffer())
    const result = await runQuery(bytes, 'SELECT 42 AS answer, 1 + 1 AS two')
    // Typed integer columns come back as JS BigInt; serialize them safely.
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
