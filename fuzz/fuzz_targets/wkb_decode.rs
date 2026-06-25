//! Fuzz the self-contained WKB geometry decoder (little-endian geometry binary).
//!
//! `wkb.rs` is wit-free (geo + byteorder + std), extracted from geomtype's
//! lib.rs so both the component and this target include it. The decoder takes
//! adversarial `&[u8]` and must never panic: malformed byte order, unknown type
//! codes, truncated coordinates, absurd element counts (a u32 ~4 billion that
//! would otherwise capacity-overflow), and deeply nested GeometryCollections
//! (stack overflow) all have to come back as `None`, never an abort.
//!
//! As a round-trip oracle we also re-encode any successfully decoded geometry;
//! `encode(decode(x))` must itself never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../extensions/geomtype-component/src/wkb.rs"]
mod wkb;

fuzz_target!(|data: &[u8]| {
    if let Some(g) = wkb::decode(data) {
        // Round-trip the decoded geometry: encoding must not panic either.
        let _ = wkb::encode(&g);
    }
});
