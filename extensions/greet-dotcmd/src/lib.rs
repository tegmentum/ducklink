//! `.greet [NAME]` — the reference pluggable dot-command component.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};
struct Component;
const FID_GREET: u64 = 1;
impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        vec![CommandSpec {
            id: FID_GREET,
            name: "greet".into(),
            summary: "Print a greeting".into(),
            usage: "greet [NAME]".into(),
        }]
    }
    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        match id {
            FID_GREET => {
                let who = args.trim();
                let who = if who.is_empty() { "world" } else { who };
                Ok(InvokeResult {
                    text: format!("hello, {who}! (from a wasm dot-command component)\n"),
                    state_deltas: vec![],
                })
            }
            other => Err(format!("greet-dotcmd: unknown command id {other}")),
        }
    }
}
export!(Component);
