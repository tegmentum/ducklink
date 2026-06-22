// Unit test for the browser TVM host (web/tvm-host.mjs). Runs in plain Node --
// no wasm needed -- by driving the same `tvm:memory` import surface the core
// component calls, including the guest's pool-and-overflow allocation pattern
// (crates/ducklink-core/src/tvm_spill.rs::alloc_in_pool).
//
// The host uses the jco import convention: functions return their value
// directly and throw `{ payload: <tvm-error> }` for the err case.
//
// Run: node web/tvm-host.test.mjs
import { createTvmHost } from './tvm-host.mjs'

let failures = 0
function check(name, cond) {
  if (cond) { console.log(`ok   - ${name}`) } else { console.error(`FAIL - ${name}`); failures++ }
}
// Returns the thrown err payload variant, or null if the call did not throw.
function caught(fn) {
  try { fn(); return null } catch (e) { return e && e.payload }
}

const { 'tvm:memory/manager': mgr, 'tvm:memory/bytes': bytes } = createTvmHost().imports

// 1) create-region returns a region id
const rid = mgr.createRegion('page-store', 1 << 20) // 1 MiB region
check('create-region returns numeric id', typeof rid === 'number')

// 2) alloc returns a well-formed handle, bumping the offset
const h0 = mgr.alloc(rid, 1000)
const h1 = mgr.alloc(rid, 2000)
check('handle has camelCased fields', 'regionId' in h0 && 'generation' in h0 && 'offset' in h0)
check('first alloc at offset 0', h0.offset === 0)
check('bump allocation advances offset', h1.offset === 1000)
check('handle points at its region', h0.regionId === rid)

// 3) write then read round-trips the exact bytes
const payload = Uint8Array.from({ length: 2000 }, (_, i) => (i * 7) & 0xff)
bytes.write(h1, payload)
const got = bytes.read(h1, payload.length)
check('round-trip length matches', got.length === payload.length)
check('round-trip bytes match', got.every((b, i) => b === payload[i]))

// 4) writes to different handles do not alias
const a = mgr.alloc(rid, 4)
const b = mgr.alloc(rid, 4)
bytes.write(a, Uint8Array.of(1, 2, 3, 4))
bytes.write(b, Uint8Array.of(9, 9, 9, 9))
check('non-aliasing writes', bytes.read(a, 4).every((x, i) => x === [1, 2, 3, 4][i]))

// 5) full region -> throws allocation-failed (drives the guest to a new region)
const small = mgr.createRegion('page-store', 8)
mgr.alloc(small, 8)
check('full region throws allocation-failed', caught(() => mgr.alloc(small, 1))?.tag === 'allocation-failed')

// 6) unknown region -> throws region-not-found
check('unknown region throws region-not-found', caught(() => mgr.alloc(4242, 1))?.tag === 'region-not-found')

// 6a) reclamation: fill a region, free everything, refill -- must reuse the
//     freed space in the SAME region (a bump allocator would be exhausted).
const reuse = mgr.createRegion('page-store', 16) // 4 x 4-byte blocks
const hs = [0, 4, 8, 12].map(() => mgr.alloc(reuse, 4))
check('region filled to capacity', caught(() => mgr.alloc(reuse, 4))?.tag === 'allocation-failed')
hs.forEach((h) => mgr.dealloc(h))
let refilled = true
for (let i = 0; i < 4; i++) { if (caught(() => mgr.alloc(reuse, 4))) refilled = false }
check('freed space is reclaimed and reused (no bump exhaustion)', refilled)
// coalescing: after freeing all, a single full-capacity alloc fits
const reuse2 = mgr.createRegion('page-store', 16)
const block = [mgr.alloc(reuse2, 8), mgr.alloc(reuse2, 8)]
block.forEach((h) => mgr.dealloc(h))
check('adjacent frees coalesce into one hole', !caught(() => mgr.alloc(reuse2, 16)))

// 6b) wrong generation -> throws stale-handle
const gh = mgr.alloc(rid, 8)
const eight = Uint8Array.of(1, 2, 3, 4, 5, 6, 7, 8)
check('wrong generation throws on write', caught(() => bytes.write({ ...gh, generation: gh.generation + 1 }, eight))?.tag === 'stale-handle')
check('wrong generation throws on read', caught(() => bytes.read({ ...gh, generation: gh.generation + 7 }, 8))?.tag === 'stale-handle')

// 6c) per-slot stale detection: a handle whose slot was freed (or freed and
//     reused by a later allocation) is rejected, not silently honored.
const slotR = mgr.createRegion('page-store', 8) // single 8-byte slot
const old = mgr.alloc(slotR, 8)                 // offset 0, generation 1
check('use-after-free is rejected', (mgr.dealloc(old), caught(() => bytes.read(old, 8))?.tag === 'stale-handle'))
const fresh = mgr.alloc(slotR, 8)               // reuses offset 0, generation 2
check('stale handle to a reused slot is rejected', caught(() => bytes.write(old, eight))?.tag === 'stale-handle')
check('the fresh handle to the reused slot works', !caught(() => bytes.write(fresh, eight)))
check('double-free is rejected', (mgr.dealloc(fresh), caught(() => mgr.dealloc(fresh))?.tag === 'stale-handle'))

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
        try { handle = m.alloc(regions[regions.length - 1], data.length) } catch { handle = null }
      }
      if (!handle) {
        const created = m.createRegion('page-store', regionCapacity)
        regions.push(created)
        handle = m.alloc(created, data.length)
      }
      by.write(handle, data)
      blocks.set(id, handle)
      return true
    },
    read: (id) => by.read(blocks.get(id), 0x10000),
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
  const r = driver.read(i)
  if (!(r.length === BLOCK && r.every((bv, j) => bv === expected[i][j]))) allMatch = false
}
check('all spilled blocks read back correctly across regions', allMatch)
const total = host.stats().bytesWritten
check(`> 2 MiB spilled to host regions (got ${(total / (1 << 20)).toFixed(1)} MiB)`, total > 2 * (1 << 20))

console.log(failures === 0 ? '\nPASS: tvm-host' : `\nFAIL: ${failures} check(s)`)
process.exit(failures === 0 ? 0 : 1)
