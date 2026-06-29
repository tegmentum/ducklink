//! Component-driven optimizer PoC (2.3.0 / v3).
//!
//! Registers an optimizer rule. At optimize time the host offers the flattened,
//! neutral plan-shape (op-type names + params-json, NOT a by-value LogicalOperator
//! tree). This rule looks for a GET on a table named `optme`; if found, it returns
//! a `rewrite-query` directive re-planning the whole query to `SELECT 99 AS
//! rewritten` -- proving the rule FIRES and the rewrite is applied end-to-end.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

// Plan-shape match logic lives in a wit-free module so the cargo-fuzz target can
// drive it natively (never-panic contract; see optimize.rs).
mod optimize;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-optimizer" });

use duckdb::extension::{optimizer, types};
use exports::duckdb::extension::{callback_dispatch, guest, optimizer_dispatch};

const RULE_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        optimizer::register_optimizer_rule("qopt", RULE_HANDLE)?;
        Ok(types::Loadresult {
            name: "qopt".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

impl optimizer_dispatch::Guest for Extension {
    fn call_optimize(
        handle: u32,
        plan: optimizer_dispatch::PlanShape,
    ) -> Result<optimizer_dispatch::RewriteDirective, types::Duckerror> {
        if handle != RULE_HANDLE {
            return Ok(optimizer_dispatch::RewriteDirective::Declined);
        }
        // Match a GET on table `optme` in the flattened plan-shape. The host packs
        // the source node JSON (incl. the table name) into each node's params-json.
        // The shape-match is the wit-free, fuzzed `optimize::matches_optme`.
        let views: Vec<optimize::NodeView> = plan
            .nodes
            .iter()
            .map(|n| optimize::NodeView {
                op_type: n.op_type.as_str(),
                params_json: n.params_json.as_str(),
            })
            .collect();
        if optimize::matches_optme(&views) {
            Ok(optimizer_dispatch::RewriteDirective::RewriteQuery(
                "SELECT 99 AS rewritten".into(),
            ))
        } else {
            Ok(optimizer_dispatch::RewriteDirective::Declined)
        }
    }
}

// Required base export; qopt has no scalar/table/aggregate/pragma/cast callbacks.
impl callback_dispatch::Guest for Extension {
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no scalar fns".into()))
    }
    // major-4 columnar dispatch: qopt is an optimizer-only component, so the
    // three columnar hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no table fns".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no casts".into()))
    }
}

export!(Extension);
