// Browser-side Tiered Virtual Memory host.
//
// Satisfies the core component's `tvm:memory/manager` + `tvm:memory/bytes`
// imports (package tvm:memory@0.1.0) with regions backed by host JS byte
// arrays, mirroring the native Rust host (crates/ducklink-host:
// RegionDirectory over VecBackedRegion). DuckDB spills evicted buffer-pool
// blocks here, so the spilled working set lives in the page's heap rather
// than the wasm32 4 GiB linear memory -- the same >4 GiB-spill capability the
// native host provides.
//
// # Return-shape convention (runtime-guest lane)
//
// The runtime-guest bindings route WIT `result<T, tvm-error>` back and forth
// as `{tag:'ok', val:T}` / `{tag:'err', val:tvmError}` OBJECTS returned by the
// impl. This differs from the previous jco lane where callers threw
// `{payload}` for the err arm — under runtime-guest, throwing is treated as a
// host-side exception and produces a trap on the guest, NOT a graceful err.
// Every path that used to `fail(errVariant)` now `return { tag: 'err', val: ...}`.
// Regions use a coalescing free-list allocator so deleted blocks are reclaimed
// (footprint tracks the live set, not cumulative spill). Single-threaded: no
// locking, unlike the native host's Mutex.

const U16_MAX = 0xffff

const ERR_ALLOC = { tag: 'allocation-failed' }
const ERR_BOUNDS = { tag: 'out-of-bounds' }
const ERR_STALE = { tag: 'stale-handle' }
const errRegion = (id) => ({ tag: 'region-not-found', val: id })

// Grow a region's backing array (doubling, capped at its capacity) so it can
// hold `need` bytes.
function ensureCapacity(region, need) {
  if (need <= region.bytes.length) return
  let size = Math.max(region.bytes.length * 2, 1 << 16)
  while (size < need) size *= 2
  if (size > region.capacity) size = region.capacity
  const grown = new Uint8Array(size)
  grown.set(region.bytes)
  region.bytes = grown
}

function newRegion(capacity) {
  return { bytes: new Uint8Array(0), capacity, free: [[0, capacity]], live: new Map(), slotGen: new Map(), used: 0 }
}
function flAlloc(region, size) {
  for (let i = 0; i < region.free.length; i++) {
    const hole = region.free[i]
    if (hole[1] >= size) {
      const offset = hole[0]
      if (hole[1] === size) region.free.splice(i, 1)
      else { hole[0] += size; hole[1] -= size }
      region.live.set(offset, size)
      region.used += size
      const generation = ((region.slotGen.get(offset) ?? 0) + 1) & 0xffff
      region.slotGen.set(offset, generation)
      return { offset, generation }
    }
  }
  return null
}
function flDealloc(region, offset) {
  const size = region.live.get(offset)
  if (size === undefined) return
  region.live.delete(offset)
  region.used -= size
  let i = 0
  while (i < region.free.length && region.free[i][0] < offset) i++
  region.free.splice(i, 0, [offset, size])
  if (i + 1 < region.free.length && region.free[i][0] + region.free[i][1] === region.free[i + 1][0]) {
    region.free[i][1] += region.free[i + 1][1]
    region.free.splice(i + 1, 1)
  }
  if (i > 0 && region.free[i - 1][0] + region.free[i - 1][1] === region.free[i][0]) {
    region.free[i - 1][1] += region.free[i][1]
    region.free.splice(i, 1)
  }
}

export function createTvmHost({ debug = false } = {}) {
  const regions = new Map()
  let nextRegionId = 0
  const stats = { regionsOpened: 0, bytesWritten: 0, bytesRead: 0 }
  const trace = (msg) => { if (debug) console.error(`[tvm] ${msg}`) }

  // Resolve a handle to its region, or return the err-variant to propagate
  // through the result<> arm. Callers check `result.err` before dereferencing.
  const regionFor = (handle) => {
    const region = regions.get(handle.regionId)
    if (!region) return { err: errRegion(handle.regionId) }
    if (!region.live.has(handle.offset) || region.slotGen.get(handle.offset) !== handle.generation) {
      return { err: ERR_STALE }
    }
    return { region }
  }

  // tvm:memory/manager@0.1.0 (runtime-guest set: createRegion / alloc /
  // dealloc; destroyRegion / describeRegion are NOT in the runtime-guest
  // interface — the wasm-cm interpreter emits providers for only what the
  // component actually imports).
  const manager = {
    createRegion(_kind, capacity) {
      if (nextRegionId > U16_MAX) return { tag: 'err', val: ERR_ALLOC }
      const id = nextRegionId++
      regions.set(id, newRegion(capacity))
      stats.regionsOpened++
      trace(`open region #${stats.regionsOpened} id=${id} cap=${capacity >> 20} MiB (host heap, beyond wasm 4 GiB)`)
      return { tag: 'ok', val: id }
    },
    alloc(regionId, size) {
      const region = regions.get(regionId)
      if (!region) return { tag: 'err', val: errRegion(regionId) }
      const slot = flAlloc(region, size)
      if (!slot) return { tag: 'err', val: ERR_ALLOC }
      ensureCapacity(region, slot.offset + size)
      return { tag: 'ok', val: { regionId, generation: slot.generation, offset: slot.offset } }
    },
    dealloc(handle) {
      const { region, err } = regionFor(handle)
      if (err) return { tag: 'err', val: err }
      flDealloc(region, handle.offset)
      return { tag: 'ok', val: undefined }
    },
  }

  // tvm:memory/bytes@0.1.0 — 2-func provider (read / write).
  const bytes = {
    write(handle, data) {
      const { region, err } = regionFor(handle)
      if (err) return { tag: 'err', val: err }
      const end = handle.offset + data.length
      if (end > region.capacity) return { tag: 'err', val: ERR_BOUNDS }
      ensureCapacity(region, end)
      region.bytes.set(data, handle.offset)
      stats.bytesWritten += data.length
      trace(`write ${data.length} B (cumulative ${stats.bytesWritten >> 20} MiB)`)
      return { tag: 'ok', val: undefined }
    },
    read(handle, len) {
      const { region, err } = regionFor(handle)
      if (err) return { tag: 'err', val: err }
      if (handle.offset + len > region.bytes.length) return { tag: 'err', val: ERR_BOUNDS }
      stats.bytesRead += len
      trace(`read ${len} B (cumulative ${stats.bytesRead >> 20} MiB)`)
      // Return a copy — subarray shares backing storage with region.bytes;
      // the router's list<u8> encoder walks the view but any subsequent
      // ensureCapacity() would invalidate a shared view. Copy is cheap
      // compared with the wire hop.
      return { tag: 'ok', val: region.bytes.slice(handle.offset, handle.offset + len) }
    },
  }

  return {
    // Shape the extension-host follows: interface-name-keyed impls the
    // runtime-guest bindings' `registerHostProviders` dispatches on.
    impls: {
      'tvm:memory/manager@0.1.0': manager,
      'tvm:memory/bytes@0.1.0': bytes,
      'tvm:memory/types@0.1.0': {},
    },
    stats: () => ({ ...stats }),
  }
}

// DUCKDB_TVM_DEBUG=1 (env in Node, or globalThis.DUCKDB_TVM_DEBUG in the browser)
// traces region opens and cumulative bytes, like the native host.
export function tvmDebugEnabled() {
  if (typeof process !== 'undefined' && process.env && process.env.DUCKDB_TVM_DEBUG) return true
  if (typeof globalThis !== 'undefined' && globalThis.DUCKDB_TVM_DEBUG) return true
  return false
}
