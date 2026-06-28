//! Component-driven optimizer PoC (2.3.0 / v3).
//!
//! Registers an optimizer rule. At optimize time the host offers the flattened,
//! neutral plan-shape (op-type names + params-json, NOT a by-value LogicalOperator
//! tree). This rule looks for a GET on a table named `optme`; if found, it returns
//! a `rewrite-query` directive re-planning the whole query to `SELECT 99 AS
//! rewritten` -- proving the rule FIRES and the rewrite is applied end-to-end.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

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
        let matched = plan.nodes.iter().any(|n| {
            let op = n.op_type.to_ascii_uppercase();
            (op.contains("GET") || op.contains("SCAN")) && n.params_json.contains("optme")
        });
        if matched {
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
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no scalar fns".into()))
    }
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("qopt: no aggregates".into()))
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
