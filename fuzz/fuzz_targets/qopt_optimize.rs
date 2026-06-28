//! Fuzz the qopt optimizer-rule plan-shape match logic (v3 @3.0.0
//! `optimizer-dispatch.call-optimize` trust boundary).
//!
//! At optimize time the host hands the rule a FLATTENED, NEUTRAL plan-shape: a
//! list of nodes each carrying an `op-type` string + a `params-json` string, all
//! attacker/core-controlled text. `optimize.rs` is wit-free (std only), so we
//! `#[path]`-include it and drive `matches_optme` from an `Arbitrary`-generated
//! list of `(op_type, params_json)` pairs: empty lists, empty/garbage/huge
//! op-types and params, non-UTF-8 (via `String`'s Arbitrary), and absurd node
//! counts.
//!
//! Contract under test: ANY node list -> a plain bool, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../extensions/qopt-component/src/optimize.rs"]
mod optimize;

fuzz_target!(|nodes: Vec<(String, String)>| {
    let views: Vec<optimize::NodeView> = nodes
        .iter()
        .map(|(op, params)| optimize::NodeView {
            op_type: op.as_str(),
            params_json: params.as_str(),
        })
        .collect();
    let _ = optimize::matches_optme(&views);
});
