//! wit-free flattening of the core's neutral plan-shape JSON into the node tuples
//! the optimizer-dispatch boundary passes to a component (v3 @3.0.0). Shared by
//! the host (`lib.rs::dispatch_optimize`) and the cargo-fuzz target
//! (`fuzz/fuzz_targets/plan_shape_parse.rs`).
//!
//! This is a v3 TRUST BOUNDARY: the wasm core flattens the bound logical plan to
//! JSON and hands it across `optimizer-host.call-optimize`. A buggy/old/adversarial
//! core (or a malformed flatten) could ship garbage; the host must turn ANY bytes
//! into either a clean node list or a clean error -- NEVER a panic (a panic here
//! aborts query optimization for every connection).
//!
//! Depends only on `serde_json` + `std`, so it compiles natively for libfuzzer.

/// A flattened plan node: (id, op-type, parent, params-json). Mirrors the tuple
/// `ducklink-runtime::ExtensionInstance::call_optimize` consumes.
pub type FlatNode = (u32, String, Option<u32>, String);

/// Parse the core's flattened plan JSON
/// (`[{"id":N,"op":"X","parent":P,"table":"T"?}, ...]`) into the neutral node
/// tuples. Returns a human-readable error string on invalid JSON; a well-formed
/// but unexpected shape (e.g. not an array, missing fields) degrades to an empty
/// / best-effort node list rather than erroring -- the rule simply sees fewer
/// nodes. Total and panic-free for any input string.
pub fn flatten_plan_json(plan_json: &str) -> Result<Vec<FlatNode>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(plan_json).map_err(|e| format!("bad plan JSON: {e}"))?;
    let mut nodes: Vec<FlatNode> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        // Bound the node count we materialize so an adversarial core that ships a
        // multi-million-element array can't make us allocate unboundedly before the
        // rule even runs. A real DuckDB plan is tiny; 1<<16 is enormous headroom.
        const MAX_NODES: usize = 1 << 16;
        for node in arr.iter().take(MAX_NODES) {
            let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let op = node
                .get("op")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parent = node
                .get("parent")
                .and_then(|v| v.as_i64())
                .filter(|p| *p >= 0)
                .map(|p| p as u32);
            // params-json carries any extra neutral fields (e.g. the table name).
            let params = node.to_string();
            nodes.push((id, op, parent, params));
        }
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_plan() {
        let json = r#"[{"id":0,"op":"LOGICAL_GET","parent":null,"table":"optme"},
                       {"id":1,"op":"LOGICAL_PROJECTION","parent":0}]"#;
        let nodes = flatten_plan_json(json).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].1, "LOGICAL_GET");
        assert_eq!(nodes[0].2, None);
        assert_eq!(nodes[1].2, Some(0));
        assert!(nodes[0].3.contains("optme"));
    }

    #[test]
    fn invalid_json_is_clean_error_not_panic() {
        assert!(flatten_plan_json("not json").is_err());
        assert!(flatten_plan_json("{").is_err());
        assert!(flatten_plan_json("").is_err());
    }

    #[test]
    fn non_array_and_missing_fields_degrade_to_empty() {
        // Valid JSON that is not the expected array shape -> empty node list, Ok.
        assert_eq!(flatten_plan_json("{}").unwrap().len(), 0);
        assert_eq!(flatten_plan_json("42").unwrap().len(), 0);
        // Array of objects missing fields -> nodes with defaults, no panic.
        let nodes = flatten_plan_json(r#"[{},{"op":123},{"parent":-5}]"#).unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].0, 0); // missing id -> 0
        assert_eq!(nodes[1].1, ""); // non-string op -> ""
        assert_eq!(nodes[2].2, None); // negative parent -> None
    }

    #[test]
    fn huge_id_and_parent_do_not_panic() {
        let json = r#"[{"id":18446744073709551615,"op":"X","parent":9223372036854775807}]"#;
        let nodes = flatten_plan_json(json).unwrap();
        assert_eq!(nodes.len(), 1);
    }
}
