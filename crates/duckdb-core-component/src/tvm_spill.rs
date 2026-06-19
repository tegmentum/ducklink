//! TVM spill bridge.
//!
//! The patched DuckDB buffer manager (`standard_buffer_manager.cpp`) calls these
//! `extern "C"` hooks to spill evicted buffer-pool blocks to host-owned TVM
//! regions (Tiered Virtual Memory) instead of temp files — extending capacity
//! beyond the wasm32 4 GiB linear-memory ceiling. Data crosses to the regions by
//! copy (`bytes::write`/`read`), never a raw pointer, so DuckDB's execution stays
//! in memory 0.
//!
//! Every hook returns 0/false to make the C++ side fall back to the default
//! temp-file path, so the component is safe even when no TVM host is wired (the
//! first `create-region` failure disables the bridge permanently).

use crate::bindings::tvm::memory::types::{Handle, RegionKind};
use crate::bindings::tvm::memory::{bytes, manager};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

// Each TVM region is one 32-bit memory (<= 4 GiB; handle offset is u32). To
// exceed 4 GiB total we pool regions and open a fresh one when the active one
// fills up. 1 GiB cap keeps each host-side allocation modest; it grows on demand.
const REGION_CAPACITY: u32 = 1 << 30;

struct Stored {
    handle: Handle,
    alloc_size: u64,   // bytes stored (== FileBuffer::AllocSize())
    logical_size: u64, // FileBuffer::size, to rebuild the buffer on read
    header_size: u64,  // FileBuffer block-header size
}

struct SpillState {
    blocks: HashMap<i64, Stored>,
    regions: Vec<u16>,
    disabled: bool,
}

fn state() -> &'static Mutex<SpillState> {
    static S: OnceLock<Mutex<SpillState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(SpillState {
            blocks: HashMap::new(),
            regions: Vec::new(),
            disabled: false,
        })
    })
}

/// Allocate `size` bytes from the region pool, opening a new region if the
/// active one is full. Returns None (and disables the bridge) if the host
/// doesn't provide TVM regions.
fn alloc_in_pool(st: &mut SpillState, size: u32) -> Option<Handle> {
    if st.disabled {
        return None;
    }
    if let Some(&rid) = st.regions.last() {
        if let Ok(h) = manager::alloc(rid, size) {
            return Some(h);
        }
    }
    match manager::create_region(RegionKind::PageStore, REGION_CAPACITY) {
        Ok(rid) => {
            st.regions.push(rid);
            manager::alloc(rid, size).ok()
        }
        Err(_) => {
            st.disabled = true;
            None
        }
    }
}

// Cached availability: 0=unknown, 1=yes, 2=no. Avoids re-probing on the hot
// CanUnload path once the answer is known.
static AVAIL: AtomicU8 = AtomicU8::new(0);

/// Can evicted blocks be spilled to TVM? DuckDB calls this from
/// `BlockHandle::CanUnload` so a buffer block with no temporary directory is
/// still evictable when a TVM host is wired. The first call probes by opening a
/// region (kept for the imminent spill); the result is cached. Returns 1 if a
/// TVM host is available, 0 otherwise (DuckDB then falls back to its temp-dir /
/// out-of-memory behavior).
#[no_mangle]
pub extern "C" fn tvm_spill_available() -> i32 {
    match AVAIL.load(Ordering::Relaxed) {
        1 => return 1,
        2 => return 0,
        _ => {}
    }
    let mut st = state().lock().unwrap();
    if st.disabled {
        AVAIL.store(2, Ordering::Relaxed);
        return 0;
    }
    if !st.regions.is_empty() {
        AVAIL.store(1, Ordering::Relaxed);
        return 1;
    }
    // CanUnload only reaches here under eviction pressure on a temp block, so a
    // spill is imminent -- open the first region now and reuse it.
    match manager::create_region(RegionKind::PageStore, REGION_CAPACITY) {
        Ok(rid) => {
            st.regions.push(rid);
            AVAIL.store(1, Ordering::Relaxed);
            1
        }
        Err(_) => {
            st.disabled = true;
            AVAIL.store(2, Ordering::Relaxed);
            0
        }
    }
}

/// Spill a block to a TVM region. Returns 1 if TVM took ownership (C++ must not
/// also write a temp file), 0 to fall back.
#[no_mangle]
pub unsafe extern "C" fn tvm_spill_write(
    _tag: u8,
    block_id: i64,
    data: *const u8,
    alloc_size: u64,
    logical_size: u64,
    header_size: u64,
) -> i32 {
    if data.is_null() || alloc_size == 0 || alloc_size > u32::MAX as u64 {
        return 0;
    }
    let mut st = state().lock().unwrap();
    let handle = match alloc_in_pool(&mut st, alloc_size as u32) {
        Some(h) => h,
        None => return 0,
    };
    let slice = std::slice::from_raw_parts(data, alloc_size as usize);
    if bytes::write(handle, slice).is_err() {
        let _ = manager::dealloc(handle);
        return 0;
    }
    st.blocks.insert(
        block_id,
        Stored {
            handle,
            alloc_size,
            logical_size,
            header_size,
        },
    );
    1
}

/// Is `block_id` in a TVM region? If so, report the sizes needed to rebuild the
/// FileBuffer before `tvm_spill_read`. Returns 1 if present.
#[no_mangle]
pub unsafe extern "C" fn tvm_spill_query(
    block_id: i64,
    out_logical: *mut u64,
    out_header: *mut u64,
) -> i32 {
    let st = state().lock().unwrap();
    match st.blocks.get(&block_id) {
        Some(s) => {
            if !out_logical.is_null() {
                *out_logical = s.logical_size;
            }
            if !out_header.is_null() {
                *out_header = s.header_size;
            }
            1
        }
        None => 0,
    }
}

/// Copy a spilled block back into `out` (capacity bytes, the rebuilt buffer's
/// AllocSize). Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn tvm_spill_read(block_id: i64, out: *mut u8, capacity: u64) -> i32 {
    if out.is_null() {
        return 0;
    }
    let (handle, alloc_size) = {
        let st = state().lock().unwrap();
        match st.blocks.get(&block_id) {
            Some(s) => (s.handle, s.alloc_size),
            None => return 0,
        }
    };
    if alloc_size > capacity {
        return 0;
    }
    match bytes::read(handle, alloc_size as u32) {
        Ok(v) => {
            let n = std::cmp::min(v.len(), capacity as usize);
            std::ptr::copy_nonoverlapping(v.as_ptr(), out, n);
            1
        }
        Err(_) => 0,
    }
}

/// Free a spilled block's region allocation. Returns the freed byte count (the
/// block's AllocSize, for the buffer manager's eviction accounting), or 0 if the
/// block wasn't in TVM.
#[no_mangle]
pub unsafe extern "C" fn tvm_spill_delete(block_id: i64) -> u64 {
    let mut st = state().lock().unwrap();
    match st.blocks.remove(&block_id) {
        Some(s) => {
            let _ = manager::dealloc(s.handle);
            s.alloc_size
        }
        None => 0,
    }
}
