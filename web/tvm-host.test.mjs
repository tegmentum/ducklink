// Unit test for the browser TVM host (web/tvm-host.mjs). Runs in plain Node --
// no wasm needed -- by driving the same `tvm:memory` import surface the core
// component calls, including the guest's pool-and-overflow allocation pattern
// (crates/duckdb-core-component/src/tvm_spill.rs::alloc_in_pool).
//
// Run: node web/tvm-host.test.mjs
import { createTvmHost } from './tvm-host.mjs'

let failures = 0
function check(name, cond) {
  if (cond) { console.log(`ok   - ${name}`) } else { console.error(`FAIL - ${name}`); failures++ }
}
const unwrap = (r) => { if (r.tag !== 'ok') throw new Error(`expected ok, got err ${JSON.stringify(r.val)}`); return r.val }

const { 'tvm:memory/manager': mgr, 'tvm:memory/bytes': bytes } = createTvmHost().imports

// 1) create-region returns an ok region id
const rid = unwrap(mgr.createRegion('page-store', 1 << 20)) // 1 MiB region
check('create-region returns numeric id', typeof rid === 'number')

// 2) alloc returns a well-formed handle, bumping the offset
const h0 = unwrap(mgr.alloc(rid, 1000))
const h1 = unwrap(mgr.alloc(rid, 2000))
check('handle has camelCased fields', 'regionId' in h0 && 'generation' in h0 && 'offset' in h0)
check('first alloc at offset 0', h0.offset === 0)
check('bump allocation advances offset', h1.offset === 1000)
check('handle points at its region', h0.regionId === rid)

// 3) write then read round-trips the exact bytes
const payload = Uint8Array.from({ length: 2000 }, (_, i) => (i * 7) & 0xff)
unwrap(bytes.write(h1, payload))
const got = unwrap(bytes.read(h1, payload.length))
check('round-trip length matches', got.length === payload.length)
check('round-trip bytes match', got.every((b, i) => b === payload[i]))

// 4) writes to different handles do not alias
const a = unwrap(mgr.alloc(rid, 4))
const b = unwrap(mgr.alloc(rid, 4))
unwrap(bytes.write(a, Uint8Array.of(1, 2, 3, 4)))
unwrap(bytes.write(b, Uint8Array.of(9, 9, 9, 9)))
check('non-aliasing writes', unwrap(bytes.read(a, 4)).every((x, i) => x === [1, 2, 3, 4][i]))

// 5) full region -> allocation-failed err (drives the guest to a new region)
const small = unwrap(mgr.createRegion('page-store', 8))
unwrap(mgr.alloc(small, 8))
const full = mgr.alloc(small, 1)
check('full region returns err', full.tag === 'err')
check('err is allocation-failed', full.val.tag === 'allocation-failed')

// 6) unknown region -> region-not-found err
const missing = mgr.alloc(4242, 1)
check('unknown region returns region-not-found', missing.tag === 'err' && missing.val.tag === 'region-not-found')

// 7) end-to-end: replicate alloc_in_pool's pool-and-overflow across regions and
//    spill > one region's capacity, proving multi-region works through the host.
function poolDriver(host, regionCapacity) {
  const m = host.imports['tvm:memory/manager']
  const by = host.imports['tvm:memory/bytes']
  const regions = []
  const blocks = new Map() // id -> handle
  return {
    write(id, data) {
      let handle = null
      if (regions.length) {
        const r = m.alloc(regions[regions.length - 1], data.length)
        if (r.tag === 'ok') handle = r.val
      }
      if (!handle) {
        const created = m.createRegion('page-store', regionCapacity)
        if (created.tag !== 'ok') return false
        regions.push(created.val)
        const r = m.alloc(created.val, data.length)
        if (r.tag !== 'ok') return false
        handle = r.val
      }
      if (by.write(handle, data).tag !== 'ok') return false
      blocks.set(id, handle)
      return true
    },
    read: (id) => unwrap(by.read(blocks.get(id), 0x10000)),
    regionCount: () => regions.length,
  }
}

const host = createTvmHost()
const driver = poolDriver(host, 1 << 20) // 1 MiB regions
const BLOCK = 0x10000 // 64 KiB blocks
const N = 40 // 40 * 64 KiB = 2.5 MiB > one 1 MiB region -> forces >=3 regions
const expected = []
for (let i = 0; i < N; i++) {
  const data = Uint8Array.from({ length: BLOCK }, (_, j) => (i * 31 + j) & 0xff)
  expected.push(data)
  check(`spill block ${i} accepted`, driver.write(i, data))
}
check('spill spanned multiple regions', driver.regionCount() >= 3)
let allMatch = true
for (let i = 0; i < N; i++) {
  const got = driver.read(i)
  if (!(got.length === BLOCK && got.every((bv, j) => bv === expected[i][j]))) allMatch = false
}
check('all spilled blocks read back correctly across regions', allMatch)
const total = host.stats().bytesWritten
check(`> 2 MiB spilled to host regions (got ${(total / (1 << 20)).toFixed(1)} MiB)`, total > 2 * (1 << 20))

console.log(failures === 0 ? '\nPASS: tvm-host' : `\nFAIL: ${failures} check(s)`)
process.exit(failures === 0 ? 0 : 1)
