//! Fuzz the WINDOW (aggregate + frame) trust boundary of the v3 @3.0.0
//! `aggregate-incr-dispatch.call-aggregate-window` surface.
//!
//! For a custom `OVER (... ROWS/RANGE BETWEEN ...)` window, the engine hands a
//! component the WHOLE partition's rows once plus a half-open `[start,end)` frame
//! (0-based row indices, `end` exclusive) per output row, and asks for the
//! aggregate over that frame. The frame bounds are `u64` chosen by the engine and
//! are fully untrusted from the component's view: they may be inverted
//! (`start > end`), out of range (`end > len`), or absurd (`u64::MAX`). A
//! component that slices `partition[start..end]` naively PANICS (a slice-bounds
//! abort kills the wasm instance).
//!
//! There is no window-aggregate component in the catalog yet (the host wiring for
//! `call-aggregate-window` is deferred -- see the v3 plan), so this target MIRRORS
//! the reference frame computation every such component MUST use: clamp the frame
//! to `[0,len)` first, then aggregate with saturating arithmetic. It is the
//! never-panic contract the future window component is required to satisfy, kept
//! here verbatim exactly like the `hex_decode` / `bencode_decode` mirrors.
//!
//! Contract: ANY `(partition, start, end)` -> a value, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

/// Clamp a half-open `[start,end)` frame to the valid `[0,len)` row range,
/// returning an in-bounds `(start,end)` with `start <= end <= len`. This is the
/// step a window component MUST perform before indexing the partition.
fn clamp_frame(len: usize, start: u64, end: u64) -> (usize, usize) {
    let len_u = len as u64;
    let s = start.min(len_u);
    let e = end.min(len_u).max(s);
    (s as usize, e as usize)
}

/// Reference SUM window aggregate over the frame. Saturating add so an
/// adversarial partition cannot overflow-panic (debug-assertions/overflow-checks
/// are ON in the fuzz profile).
fn window_sum(partition: &[i64], start: u64, end: u64) -> i64 {
    let (s, e) = clamp_frame(partition.len(), start, end);
    partition[s..e].iter().fold(0i64, |acc, &x| acc.saturating_add(x))
}

/// Reference COUNT window aggregate over the frame.
fn window_count(len: usize, start: u64, end: u64) -> u64 {
    let (s, e) = clamp_frame(len, start, end);
    (e - s) as u64
}

/// Reference MIN/MAX window aggregate (returns None for an empty frame -- the
/// SQL-correct NULL).
fn window_minmax(partition: &[i64], start: u64, end: u64) -> Option<(i64, i64)> {
    let (s, e) = clamp_frame(partition.len(), start, end);
    let slice = &partition[s..e];
    let mn = slice.iter().min()?;
    let mx = slice.iter().max()?;
    Some((*mn, *mx))
}

fuzz_target!(|input: (Vec<i64>, u64, u64)| {
    let (partition, start, end) = input;
    // None of these may panic for any frame -- inverted, out-of-range, or huge.
    let sum = window_sum(&partition, start, end);
    let cnt = window_count(partition.len(), start, end);
    let mm = window_minmax(&partition, start, end);

    // Cross-check the clamp invariants (also caught by overflow-checks).
    let (s, e) = clamp_frame(partition.len(), start, end);
    assert!(s <= e && e <= partition.len());
    // An empty frame sums to 0 / counts 0 / has no min-max.
    if s == e {
        assert_eq!(sum, 0);
        assert_eq!(cnt, 0);
        assert!(mm.is_none());
    } else {
        assert_eq!(cnt as usize, e - s);
        assert!(mm.is_some());
    }
});
