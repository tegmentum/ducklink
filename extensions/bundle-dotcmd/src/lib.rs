//! The `.bundle` dot command — embedding tracking, sqlink's "Bundles" surface
//! adapted to ducklink. A *build* (aka bundle) is a NAMED set of content-hashed
//! embedding members (the core's EMBED_EXTENSIONS set + the loaded/composed
//! component extensions), keyed by a `set_hash`. The persisted records live in
//! `registry/builds.json`, managed by `tooling/builds.py`.
//!
//! What this dotcmd CAN do on the LEAN core (no json extension, no file reads):
//!   .bundle loaded            — introspect the LIVE set of loaded extensions via
//!                               `spi::query(duckdb_extensions())`. This is the
//!                               sqlink `.bundle save` *introspection* step — the
//!                               raw material you'd record into a build.
//!   .bundle members           — the live set rendered as the `name<TAB>set` lines
//!                               that feed a set_hash (mirrors sqlink's member set).
//!   .bundle help / .bundle    — explain the model and point at the recorder.
//!
//! Why not a live `.bundle list` of saved builds? The dotcmd has no filesystem
//! access, and `read_json('registry/builds.json')` needs the JSON extension which
//! is DE-EMBEDDED from the lean core (json is loadable-on-demand, not built in).
//! So the persisted-record surface (list/show/record/gen/verify) lives in
//! `tooling/builds.py`; `.bundle` covers the live-introspection half that a
//! dotcmd is actually positioned to do, and points at the tool for the rest.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};
use duckdb::dotcmd::spi;

struct Component;
const FID_BUNDLE: u64 = 1; // bare ".bundle" / ".bundle help"
const FID_LOADED: u64 = 2; // ".bundle loaded"
const FID_MEMBERS: u64 = 3; // ".bundle members"

fn plain(text: std::string::String) -> InvokeResult {
    InvokeResult { text, state_deltas: vec![] }
}

const HELP: &str = "\
.bundle — embedding tracking (what builds embed which extensions)\n\
\n\
  .bundle loaded     live set of loaded extensions (the introspection a\n\
                     `record` would capture: name + loaded/installed flags)\n\
  .bundle members    the live set as `name<TAB>set` member lines (the rows\n\
                     that feed a build's set_hash)\n\
  .bundle help       this help\n\
\n\
Persisted build records live in registry/builds.json, managed by the\n\
recorder/query tool (the dotcmd has no file access on the lean core):\n\
\n\
  python3 tooling/builds.py record <name> --embed core_functions,parquet \\\n\
                                   --component jsonfns@artifacts/extensions/jsonfns.wasm\n\
  python3 tooling/builds.py list                 # table of builds\n\
  python3 tooling/builds.py show <name>          # full detail\n\
  python3 tooling/builds.py gen                  # (re)write BUILDS.md\n\
  python3 tooling/builds.py verify               # consistency check\n";

impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        let c = |id, name: &str, summary: &str, usage: &str| CommandSpec {
            id, name: name.into(), summary: summary.into(), usage: usage.into(),
        };
        vec![c(
            FID_BUNDLE,
            "bundle",
            "Embedding tracking: what builds embed which extensions",
            "bundle [loaded|members|help]",
        )]
    }

    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        // The host routes ".bundle ARGS" to FID_BUNDLE; we sub-dispatch on the
        // first word so the single registered name carries subcommands (sqlink's
        // `.bundle <verb>` shape) without needing per-verb registry entries.
        let arg = args.trim();
        let (verb, _rest) = match arg.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (arg, ""),
        };
        let fid = match verb {
            "" | "help" => FID_BUNDLE,
            "loaded" => FID_LOADED,
            "members" => FID_MEMBERS,
            other => {
                return Err(format!(
                    "unknown .bundle subcommand '{other}' (try: loaded, members, help)"
                ))
            }
        };
        let _ = id;
        match fid {
            FID_BUNDLE => Ok(plain(HELP.to_string())),
            FID_LOADED => {
                // duckdb_extensions() is a CORE builtin table function — present
                // on the lean core, no json needed.
                let out = spi::query(
                    "SELECT extension_name, loaded, installed \
                     FROM duckdb_extensions() \
                     WHERE loaded OR installed ORDER BY extension_name",
                )?;
                Ok(plain(if out.trim().is_empty() {
                    "(no extensions reported loaded/installed)\n".to_string()
                } else {
                    out
                }))
            }
            FID_MEMBERS => {
                // Render the live loaded set as set_hash member lines:
                // `name<TAB>set` (set = the placeholder hash a recorder would
                // fill with the artifact content hash). This mirrors the sorted
                // member lines that sqlink/builds.py feed into the set_hash.
                let out = spi::query(
                    "SELECT extension_name || chr(9) || 'set' \
                     FROM duckdb_extensions() WHERE loaded \
                     ORDER BY extension_name",
                )?;
                let body = if out.trim().is_empty() {
                    "(no loaded extensions)".to_string()
                } else {
                    out.trim_end().to_string()
                };
                Ok(plain(format!(
                    "# live member set (name<TAB>placeholder). Record with:\n\
                     #   python3 tooling/builds.py record <name> --component <ext>@<artifact>\n\
                     {body}\n"
                )))
            }
            _ => Err("bundle-dotcmd: unreachable command id".to_string()),
        }
    }
}
export!(Component);
