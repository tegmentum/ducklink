//! wit-free plan-shape match logic for qopt (optimizer-dispatch surface, v3
//! @3.0.0), shared VERBATIM by the component (`lib.rs`) and the cargo-fuzz target
//! (`fuzz/fuzz_targets/qopt_optimize.rs`).
//!
//! This is a v3 TRUST BOUNDARY: the host hands the component a FLATTENED, NEUTRAL
//! plan-shape (per node: an `op-type` string + a `params-json` string), all of
//! which is attacker/core-controlled text. The contract under test: NEVER PANIC
//! on any node list -- empty op-types, huge/garbage/non-UTF-8 op-types and
//! params-json, absurd node counts must all return a plain bool, never an abort.
//!
//! No wit types appear here, so the file is natively compilable for libfuzzer.

/// One flattened plan node, in wit-free terms: just the two text fields the rule
/// inspects (the `op-type` name and the neutral `params-json` blob).
#[derive(Debug, Clone)]
pub struct NodeView<'a> {
    pub op_type: &'a str,
    pub params_json: &'a str,
}

/// Does any node in the flattened plan look like a GET/SCAN over a table named
/// `optme`? Pure, total, panic-free for any node list (this is the qopt PoC
/// rule's whole shape-match).
pub fn matches_optme(nodes: &[NodeView<'_>]) -> bool {
    nodes.iter().any(|n| {
        let op = n.op_type.to_ascii_uppercase();
        (op.contains("GET") || op.contains("SCAN")) && n.params_json.contains("optme")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv<'a>(op: &'a str, params: &'a str) -> NodeView<'a> {
        NodeView { op_type: op, params_json: params }
    }

    #[test]
    fn matches_get_on_optme() {
        let nodes = [nv("LOGICAL_GET", "{\"table\":\"optme\"}")];
        assert!(matches_optme(&nodes));
    }

    #[test]
    fn declines_other_tables_and_ops() {
        assert!(!matches_optme(&[nv("LOGICAL_GET", "{\"table\":\"other\"}")]));
        assert!(!matches_optme(&[nv("LOGICAL_PROJECTION", "{\"table\":\"optme\"}")]));
        assert!(!matches_optme(&[]));
    }

    #[test]
    fn empty_and_garbage_fields_do_not_panic() {
        let nodes = [
            nv("", ""),
            nv("\u{1F4A9}SCAN", "optme\u{0}\u{1F4A9}"),
            nv("scan", "OPTME"), // case: params match is case-sensitive
        ];
        // Just must not panic; lower-case "scan" upper-cases to SCAN and the
        // exact substring "optme" is present in node 2's params.
        let _ = matches_optme(&nodes);
        assert!(matches_optme(&[nv("a_scan_b", "...optme...")]));
    }
}
