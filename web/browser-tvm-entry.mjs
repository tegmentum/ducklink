// Browser demo: DuckDB's larger-than-memory spill flowing through the browser
// TVM host. Set a low memory_limit and no temp_directory, then run a sort that
// exceeds the budget -- the evicted blocks spill into the host's JS-owned TVM
// regions (web/tvm-host.mjs), which live in the page heap beyond the wasm32
// 4 GiB ceiling. Reports the sorted result plus the host's region/byte stats,
// proving >memory_limit data round-tripped through TVM. Open with #2GB to
// disable spilling as an in-memory control.
import { instantiateCore } from './run-core.mjs'

async function main() {
  const out = document.getElementById('out')
  out.dataset.status = 'running'
  try {
    const resp = await fetch('./duckdb_core_component.wasm')
    const bytes = new Uint8Array(await resp.arrayBuffer())
    const db = await instantiateCore(bytes)
    const conn = db.open(undefined) // in-memory

    // memory_limit from the URL hash (#2GB disables spilling as a control).
    const limit = (location.hash || '#64MB').slice(1)
    // No temp_directory: TVM availability alone must make blocks evictable.
    db.execute(conn, `SET memory_limit='${limit}'`)
    db.execute(conn, 'SET threads=1')
    // 10M int64 sorted ~= 80 MiB > the 64 MiB limit -> forces a spill.
    const result = db.execute(
      conn,
      'SELECT count(*) AS n, min(i) AS lo, max(i) AS hi ' +
        'FROM (SELECT i FROM range(10000000) t(i) ORDER BY i DESC) sub',
    )
    db.close(conn)

    const stats = db.__tvmHost.stats()
    const ser = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? `${v}n` : v))
    const writtenMiB = (stats.bytesWritten / (1 << 20)).toFixed(1)
    const readMiB = (stats.bytesRead / (1 << 20)).toFixed(1)
    const row = result.rows[0]
    const n = row[0].val, lo = row[1].val, hi = row[2].val
    const correct = String(n) === '10000000' && String(lo) === '0' && String(hi) === '9999999'

    out.textContent =
      'result: ' + ser(result.rows) + '\n' +
      `correct (n=10M, min=0, max=9999999): ${correct}\n` +
      `tvm regions opened: ${stats.regionsOpened}\n` +
      `tvm bytes written:  ${writtenMiB} MiB\n` +
      `tvm bytes read:     ${readMiB} MiB`
    out.dataset.status = correct ? 'ok' : 'error'
  } catch (e) {
    out.textContent = 'ERROR: ' + (e && (e.stack || e.message) || e)
    out.dataset.status = 'error'
  }
}

main()
