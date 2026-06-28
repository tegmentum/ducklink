//! Fuzz the host-side plan-shape JSON flattening (v3 @3.0.0 optimizer-dispatch
//! trust boundary, HOST side).
//!
//! The wasm core flattens the bound logical plan to JSON and hands it across
//! `optimizer-host.call-optimize`; the host parses it into neutral node tuples
//! before driving the component rule. `plan_shape.rs` depends only on serde_json
//! + std, so we `#[path]`-include it and drive `flatten_plan_json` with the
//! libfuzzer buffer as a (possibly invalid) JSON string: malformed JSON, valid
//! JSON of the wrong shape, missing/garbage fields, huge ids/parents, and large
//! node arrays.
//!
//! Contract under test: ANY input -> `Ok(nodes)` or `Err(msg)`, never a panic. A
//! panic here aborts query optimization for every connection.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/ducklink-host/src/plan_shape.rs"]
mod plan_shape;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = plan_shape::flatten_plan_json(&s);
});
