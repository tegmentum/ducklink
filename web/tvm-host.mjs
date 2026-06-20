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
// jco conventions used here:
//   - result<T,E>           -> return { tag: 'ok', val } | { tag: 'err', val }
//   - record handle         -> { regionId, generation, offset } (camelCased)
//   - variant tvm-error      -> { tag: '<case>', val? }
//   - list<u8>               -> Uint8Array
// Single-threaded: no locking, unlike the native host's Mutex.

const U16_MAX = 0xffff

const ok = (val) => ({ tag: 'ok', val })
const err = (val) => ({ tag: 'err', val })
const ERR_ALLOC = { tag: 'allocation-failed' }
const ERR_BOUNDS = { tag: 'out-of-bounds' }
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

export function createTvmHost({ debug = false } = {}) {
  // region-id (u16) -> { bytes, used (bump pointer), capacity, generation }
  const regions = new Map()
  let nextRegionId = 0
  const stats = { regionsOpened: 0, bytesWritten: 0, bytesRead: 0 }
  const trace = (msg) => { if (debug) console.error(`[tvm] ${msg}`) }

  const manager = {
    // Each region is one logical 32-bit address space (offset is u32). The guest
    // pools regions and opens a fresh one when the active one fills, so total
    // spill capacity is multi-region and exceeds 4 GiB.
    createRegion(_kind, capacity) {
      if (nextRegionId > U16_MAX) return err(ERR_ALLOC)
      const id = nextRegionId++
      regions.set(id, { bytes: new Uint8Array(0), used: 0, capacity, generation: 0 })
      stats.regionsOpened++
      trace(`open region #${stats.regionsOpened} id=${id} cap=${capacity >> 20} MiB (host heap, beyond wasm 4 GiB)`)
      return ok(id)
    },
    destroyRegion(regionId) {
      return regions.delete(regionId) ? ok(undefined) : err(errRegion(regionId))
    },
    alloc(regionId, size) {
      const region = regions.get(regionId)
      if (!region) return err(errRegion(regionId))
      // Region full -> err so the guest opens another region (matches native).
      if (region.used + size > region.capacity) return err(ERR_ALLOC)
      const offset = region.used
      ensureCapacity(region, offset + size)
      region.used = offset + size
      return ok({ regionId, generation: region.generation, offset })
    },
    // Bump allocator: individual frees are no-ops; memory is reclaimed when the
    // whole region is destroyed (same as the native VecBackedRegion).
    dealloc(_handle) {
      return ok(undefined)
    },
    describeRegion(regionId) {
      const region = regions.get(regionId)
      if (!region) return err(errRegion(regionId))
      return ok({
        id: regionId,
        generation: region.generation,
        kind: 'page-store',
        capacity: region.capacity,
        used: region.used,
        residency: 'cold',
      })
    },
  }

  const bytes = {
    write(handle, data) {
      const region = regions.get(handle.regionId)
      if (!region) return err(errRegion(handle.regionId))
      const end = handle.offset + data.length
      if (end > region.capacity) return err(ERR_BOUNDS)
      ensureCapacity(region, end)
      region.bytes.set(data, handle.offset)
      stats.bytesWritten += data.length
      trace(`write ${data.length} B (cumulative ${stats.bytesWritten >> 20} MiB)`)
      return ok(undefined)
    },
    read(handle, len) {
      const region = regions.get(handle.regionId)
      if (!region) return err(errRegion(handle.regionId))
      if (handle.offset + len > region.bytes.length) return err(ERR_BOUNDS)
      stats.bytesRead += len
      trace(`read ${len} B (cumulative ${stats.bytesRead >> 20} MiB)`)
      return ok(region.bytes.slice(handle.offset, handle.offset + len))
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
