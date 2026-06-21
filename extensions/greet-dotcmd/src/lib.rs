//! `.greet [NAME]` — the reference pluggable dot-command component. Proves the
//! host-mediated dispatch: the duckdb CLI routes an unknown `.NAME` to the host,
//! which invokes this component's `registry.invoke`.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest};
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
    fn invoke(id: u64, args: String) -> Result<String, String> {
        match id {
            FID_GREET => {
                let who = args.trim();
                let who = if who.is_empty() { "world" } else { who };
                Ok(format!("hello, {who}! (from a wasm dot-command component)\n"))
            }
            other => Err(format!("greet-dotcmd: unknown command id {other}")),
        }
    }
}
export!(Component);
