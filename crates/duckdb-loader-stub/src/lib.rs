// No-op loader for the standalone (wac-composed) deployment.
//
// The core component imports three interfaces so the native host can load
// extension *components* at runtime: `host-extension-loader` (request a load),
// `extension-loader-hooks` (drain captured registrations), and
// `callback-dispatch` (invoke an extension's callbacks). A standalone wasm has
// no component runtime inside it, so it cannot instantiate extensions; this
// stub satisfies those imports with declining/empty implementations. The
// dispatch entry points are unreachable in practice — nothing ever registers a
// callback handle — but must exist to type-check the composition.
#[allow(warnings)]
mod bindings;

use bindings::exports::duckdb::component::extension_loader_hooks::{
    Guest as HooksGuest, PendingRegistrations,
};
use bindings::exports::duckdb::component::host_extension_loader::Guest as LoaderGuest;
use bindings::exports::duckdb::extension::callback_dispatch::{
    Duckerror, Duckvalue, Guest as DispatchGuest, Invokeinfo, Resultset, Rowbatch,
};

struct Component;

impl LoaderGuest for Component {
    fn request_load(_name: String) -> bool {
        // No extensions are linkable in a standalone build.
        false
    }
}

impl HooksGuest for Component {
    fn get_pending_registrations() -> PendingRegistrations {
        PendingRegistrations {
            scalars: Vec::new(),
            tables: Vec::new(),
            aggregates: Vec::new(),
            macros: Vec::new(),
            replacement_scans: Vec::new(),
            logical_types: Vec::new(),
            casts: Vec::new(),
        }
    }
}

fn unreachable_dispatch() -> Duckerror {
    Duckerror::Internal("standalone build has no loadable extensions".to_string())
}

impl DispatchGuest for Component {
    fn call_scalar(
        _handle: u32,
        _args: Vec<Duckvalue>,
        _ctx: Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_table(_handle: u32, _args: Vec<Duckvalue>) -> Result<Resultset, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_aggregate(_handle: u32, _rows: Rowbatch) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<Duckvalue>,
    ) -> Result<Option<Duckvalue>, Duckerror> {
        Err(unreachable_dispatch())
    }

    fn call_cast(_handle: u32, _value: Duckvalue) -> Result<Duckvalue, Duckerror> {
        Err(unreachable_dispatch())
    }
}

bindings::export!(Component with_types_in bindings);
