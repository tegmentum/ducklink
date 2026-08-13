// Unit test for the browser TVM host (web/tvm-host.mjs). Runs in plain Node --
// no wasm needed -- by driving the same `tvm:memory` import surface the core
// component calls, including the guest's pool-and-overflow allocation pattern
// (crates/ducklink-core/src/tvm_spill.rs::alloc_in_pool).
//
// Runtime-guest convention: manager / bytes methods return `{tag:'ok', val}`
// on success and `{tag:'err', val:<variant>}` on failure — the runtime-guest
// router encodes these directly onto the wire. (The previous jco lane used
// `throw {payload}` for the err arm; this test asserts against the new shape.)
//
// Run: node web/tvm-host.test.mjs
import { createTvmHost } from './tvm-host.mjs'

let failures = 0
function check(name, cond) {
  if (cond) { console.log(`ok   - ${name}`) } else { console.error(`FAIL - ${name}`); failures++ }
}
// Assert a call returned an ok arm; returns the unwrapped value.
function ok(res, label) {
  if (!res || res.tag !== 'ok') {
    console.error(`FAIL - ${label}: expected ok, got ${JSON.stringify(res)}`)
    failures++
    return undefined
  }
  return res.val
}
// Assert a call returned an err arm with the expected tag on the err variant.
function errTag(res) {
  return res && res.tag === 'err' ? res.val?.tag : null
}

const host = createTvmHost()
const mgr = host.impls['tvm:memory/manager@0.1.0']
const bytes = host.impls['tvm:memory/bytes@0.1.0']

// 1) create-region returns a region id
const rid = ok(mgr.createRegion('page-store', 1 << 20), 'create-region')
check('create-region returns numeric id', typeof rid === 'number')

// 2) alloc returns a well-formed handle, bumping the offset
const h0 = ok(mgr.alloc(rid, 1000), 'alloc h0')
const h1 = ok(mgr.alloc(rid, 2000), 'alloc h1')
check('handle has camelCased fields', 'regionId' in h0 && 'generation' in h0 && 'offset' in h0)
check('first alloc at offset 0', h0.offset === 0)
check('bump allocation advances offset', h1.offset === 1000)
check('handle points at its region', h0.regionId === rid)

// 3) write then read round-trips the exact bytes
const payload = Uint8Array.from({ length: 2000 }, (_, i) => (i * 7) & 0xff)
ok(bytes.write(h1, payload), 'write h1')
const got = ok(bytes.read(h1, payload.length), 'read h1')
check('round-trip length matches', got.length === payload.length)
check('round-trip bytes match', got.every((b, i) => b === payload[i]))

// 4) writes to different handles do not alias
const a = ok(mgr.alloc(rid, 4), 'alloc a')
const b = ok(mgr.alloc(rid, 4), 'alloc b')
ok(bytes.write(a, Uint8Array.of(1, 2, 3, 4)), 'write a')
ok(bytes.write(b, Uint8Array.of(9, 9, 9, 9)), 'write b')
const gotA = ok(bytes.read(a, 4), 'read a')
check('non-aliasing writes', gotA.every((x, i) => x === [1, 2, 3, 4][i]))

// 5) full region -> err allocation-failed (drives the guest to a new region)
const small = ok(mgr.createRegion('page-store', 8), 'create small')
ok(mgr.alloc(small, 8), 'fill small')
check('full region errs allocation-failed', errTag(mgr.alloc(small, 1)) === 'allocation-failed')

// 6) unknown region -> err region-not-found
check('unknown region errs region-not-found', errTag(mgr.alloc(4242, 1)) === 'region-not-found')

// 6a) reclamation: fill a region, free everything, refill.
const reuse = ok(mgr.createRegion('page-store', 16), 'create reuse')
const hs = [0, 4, 8, 12].map(() => ok(mgr.alloc(reuse, 4), 'reuse alloc'))
check('region filled to capacity', errTag(mgr.alloc(reuse, 4)) === 'allocation-failed')
hs.forEach((h) => ok(mgr.dealloc(h), 'dealloc'))
let refilled = true
for (let i = 0; i < 4; i++) { if (mgr.alloc(reuse, 4).tag !== 'ok') refilled = false }
check('freed space is reclaimed and reused (no bump exhaustion)', refilled)
const reuse2 = ok(mgr.createRegion('page-store', 16), 'create reuse2')
const block = [ok(mgr.alloc(reuse2, 8), 'reuse2 a'), ok(mgr.alloc(reuse2, 8), 'reuse2 b')]
block.forEach((h) => ok(mgr.dealloc(h), 'dealloc reuse2'))
check('adjacent frees coalesce into one hole', mgr.alloc(reuse2, 16).tag === 'ok')

// 6b) wrong generation -> err stale-handle
const gh = ok(mgr.alloc(rid, 8), 'alloc gh')
const eight = Uint8Array.of(1, 2, 3, 4, 5, 6, 7, 8)
check('wrong generation errs on write', errTag(bytes.write({ ...gh, generation: gh.generation + 1 }, eight)) === 'stale-handle')
check('wrong generation errs on read', errTag(bytes.read({ ...gh, generation: gh.generation + 7 }, 8)) === 'stale-handle')

// 6c) per-slot stale detection: a handle whose slot was freed (or freed and
//     reused by a later allocation) is rejected, not silently honored.
const slotR = ok(mgr.createRegion('page-store', 8), 'create slotR')
const oldH = ok(mgr.alloc(slotR, 8), 'alloc oldH')
ok(mgr.dealloc(oldH), 'dealloc oldH')
check('use-after-free is rejected', errTag(bytes.read(oldH, 8)) === 'stale-handle')
const fresh = ok(mgr.alloc(slotR, 8), 'alloc fresh')
check('stale handle to a reused slot is rejected', errTag(bytes.write(oldH, eight)) === 'stale-handle')
check('the fresh handle to the reused slot works', bytes.write(fresh, eight).tag === 'ok')
ok(mgr.dealloc(fresh), 'dealloc fresh')
check('double-free is rejected', errTag(mgr.dealloc(fresh)) === 'stale-handle')

// 7) end-to-end: replicate alloc_in_pool's pool-and-overflow across regions and
//    spill > one region's capacity, proving multi-region works through the host.
function poolDriver(host, regionCapacity) {
  const m = host.impls['tvm:memory/manager@0.1.0']
  const by = host.impls['tvm:memory/bytes@0.1.0']
  const regions = []
  const blocks = new Map()
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
      const w = by.write(handle, data)
      if (w.tag !== 'ok') return false
      blocks.set(id, handle)
      return true
    },
    read: (id) => {
      const r = by.read(blocks.get(id), 0x10000)
      return r.tag === 'ok' ? r.val : new Uint8Array(0)
    },
    regionCount: () => regions.length,
  }
}

const host2 = createTvmHost()
const driver = poolDriver(host2, 1 << 20)
const BLOCK = 0x10000
const N = 40
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
const total = host2.stats().bytesWritten
check(`> 2 MiB spilled to host regions (got ${(total / (1 << 20)).toFixed(1)} MiB)`, total > 2 * (1 << 20))

console.log(failures === 0 ? '\nPASS: tvm-host' : `\nFAIL: ${failures} check(s)`)
process.exit(failures === 0 ? 0 : 1)
