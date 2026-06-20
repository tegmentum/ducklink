// Browser-side Tiered Virtual Memory host.
//
// Satisfies the core component's `tvm:memory/manager` + `tvm:memory/bytes`
// imports (package tvm:memory@0.1.0) with regions backed by host JS byte arrays,
// mirroring the native Rust host (crates/duckdb-component-host: RegionDirectory
// over VecBackedRegion). DuckDB spills evicted buffer-pool blocks here, so the
// spilled working set lives in the page's heap rather than the wasm32 4 GiB
// linear memory -- the same >4 GiB-spill capability the native host provides.
//
// These imports are UNCONDITIONAL in the component world, so the browser build
// cannot instantiate without them wired (even for queries that never spill).
//
// jco import conventions (both RuntimeBindgen and `jco transpile` use these):
//   - result<T,E>  -> return T directly on success; for the err case THROW
//                     `{ payload: <E> }` (jco wraps the return as ok and reads
//                     `e.payload` as the err value; a bare `Error` re-throws).
//                     Returning `{tag:'ok',val}` yourself double-wraps and
//                     corrupts the value -- do not.
//   - record handle -> { regionId, generation, offset } (jco camelCases fields)
//   - variant tvm-error -> { tag: '<case>', val? }
//   - list<u8>      -> Uint8Array
// Regions use a coalescing free-list allocator so deleted blocks are reclaimed
// (footprint tracks the live set, not cumulative spill). Single-threaded: no
// locking, unlike the native host's Mutex.

const U16_MAX = 0xffff

// Signal a result's err case: throw a `{ payload }` jco unwraps into the err.
const fail = (variant) => { throw { payload: variant } }
const ERR_ALLOC = { tag: 'allocation-failed' }
const ERR_BOUNDS = { tag: 'out-of-bounds' }
const ERR_STALE = { tag: 'stale-handle' }
const errRegion = (id) => ({ tag: 'region-not-found', val: id })

// Grow a region's backing array (doubling, capped at its capacity) so it can
// hold `need` bytes. Regions start empty and grow on demand -- the guest asks
// for 1 GiB regions but rarely fills them, so reserving up front would waste
// page memory.
function ensureCapacity(region, need) {
  if (need <= region.bytes.length) return
  let size = Math.max(region.bytes.length * 2, 1 << 16)
  while (size < need) size *= 2
  if (size > region.capacity) size = region.capacity
  const grown = new Uint8Array(size)
  grown.set(region.bytes)
  region.bytes = grown
}

// Coalescing free-list allocator (mirrors tvm_core's FreelistAllocator). Holes
// are `[offset, size]` pairs kept sorted by offset; `live` maps an allocation's
// offset to its size so dealloc can reclaim it. Reclamation is what keeps a
// region's footprint at the live set rather than the cumulative spill volume:
// DuckDB deletes spilled blocks as a sort/hash merge consumes them.
function newRegion(capacity, generation) {
  return { bytes: new Uint8Array(0), capacity, generation, free: [[0, capacity]], live: new Map(), used: 0 }
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
      return offset
    }
  }
  return -1 // no contiguous hole big enough
}
function flDealloc(region, offset) {
  const size = region.live.get(offset)
  if (size === undefined) return // already free / unknown -- ignore
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
  const regions = new Map() // region-id -> region (see newRegion)
  let nextRegionId = 0
  const stats = { regionsOpened: 0, bytesWritten: 0, bytesRead: 0 }
  const trace = (msg) => { if (debug) console.error(`[tvm] ${msg}`) }
  // Reject a handle whose region is gone or whose generation is stale (region
  // ids never repeat here, so this mostly mirrors the native generation check).
  const regionFor = (handle) => {
    const region = regions.get(handle.regionId)
    if (!region) fail(errRegion(handle.regionId))
    if (handle.generation !== region.generation) fail(ERR_STALE)
    return region
  }

  const manager = {
    // Each region is one logical 32-bit address space (offset is u32). The guest
    // pools regions and opens a fresh one when the active one fills, so total
    // spill capacity is multi-region and exceeds 4 GiB.
    createRegion(_kind, capacity) {
      if (nextRegionId > U16_MAX) fail(ERR_ALLOC)
      const id = nextRegionId++
      regions.set(id, newRegion(capacity, 1))
      stats.regionsOpened++
      trace(`open region #${stats.regionsOpened} id=${id} cap=${capacity >> 20} MiB (host heap, beyond wasm 4 GiB)`)
      return id
    },
    destroyRegion(regionId) {
      if (!regions.delete(regionId)) fail(errRegion(regionId))
    },
    alloc(regionId, size) {
      const region = regions.get(regionId)
      if (!region) fail(errRegion(regionId))
      const offset = flAlloc(region, size)
      // No hole big enough -> err so the guest opens another region (matches native).
      if (offset < 0) fail(ERR_ALLOC)
      ensureCapacity(region, offset + size)
      return { regionId, generation: region.generation, offset }
    },
    // Reclaim the block's space back into the region's free list.
    dealloc(handle) {
      flDealloc(regionFor(handle), handle.offset)
    },
    // Declared by the interface but never called by the guest spill bridge.
    describeRegion(regionId) {
      const region = regions.get(regionId)
      if (!region) fail(errRegion(regionId))
      return {
        id: regionId, generation: region.generation, kind: 'page-store',
        capacity: region.capacity, used: region.used, residency: 'cold',
      }
    },
  }

  const bytes = {
    write(handle, data) {
      const region = regionFor(handle)
      const end = handle.offset + data.length
      if (end > region.capacity) fail(ERR_BOUNDS)
      ensureCapacity(region, end)
      region.bytes.set(data, handle.offset)
      stats.bytesWritten += data.length
      trace(`write ${data.length} B (cumulative ${stats.bytesWritten >> 20} MiB)`)
    },
    read(handle, len) {
      const region = regionFor(handle)
      if (handle.offset + len > region.bytes.length) fail(ERR_BOUNDS)
      stats.bytesRead += len
      trace(`read ${len} B (cumulative ${stats.bytesRead >> 20} MiB)`)
      return region.bytes.slice(handle.offset, handle.offset + len)
    },
  }

  return {
    imports: {
      'tvm:memory/manager': manager,
      'tvm:memory/bytes': bytes,
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
