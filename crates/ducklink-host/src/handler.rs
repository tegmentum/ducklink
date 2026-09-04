//! Request-handler component support for duckdb-wasm-httpd.
//!
//! Implements the host side of the `duckdb:handler/request-handler` world:
//! load wasm components (`--load NAME=PATH`) that export
//! `handler.handle(request: string) -> result<string, string>`, and invoke one
//! per HTTP request whose route has `kind='wasm'`. Mirrors sqlite-wasm's
//! `language-runtime` dispatcher: each call gets a FRESH wasmtime Store, so
//! handlers are stateless across requests (persistent state belongs in the DB).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use wasmos_runtime_api::Value;
use wasmos_runtime_wasmtime_v48::sync_export_bridge;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{AsContextMut, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::build_engine;

// Under the Path-A migration (see `docs/wasmos-migration-recipe.md`)
// the `wasmtime::component::bindgen!` invocation that used to sit here
// is gone. The single WIT export — `duckdb:handler/handler.handle` —
// is dispatched dynamically through `sync_export_bridge::call_export`
// with hand-marshalled `wasmos_runtime_api::Value` args + returns.
// WASI plumbing stays on the wasmtime side (unchanged from bindgen
// era) — the bridge only replaces the typed dispatcher, not the
// linker or store construction.

struct HandlerStoreState {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for HandlerStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Loaded request-handler components, keyed by the name given to `--load`.
pub struct HandlerRegistry {
    engine: Engine,
    handlers: HashMap<String, Component>,
    env: Vec<(String, String)>,
}

impl HandlerRegistry {
    /// Build an empty registry. `env` is the set of env vars forwarded into
    /// every handler invocation (no process env is exposed otherwise).
    pub fn new(env: Vec<(String, String)>) -> Result<Self> {
        Ok(Self {
            engine: build_engine()?,
            handlers: HashMap::new(),
            env,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Compile + register a handler component under `name`.
    pub fn register(&mut self, name: &str, path: &Path) -> Result<()> {
        let component = Component::from_file(&self.engine, path)
            .map_err(|e| e.context(format!("load handler component {}", path.display())))?;
        self.handlers.insert(name.to_string(), component);
        Ok(())
    }

    /// Invoke the named handler with `request_json`. Returns the handler's
    /// `Ok(body)` / `Err(message)`. A fresh Store per call keeps handlers
    /// stateless across requests.
    pub fn invoke(
        &self,
        name: &str,
        request_json: &str,
    ) -> Result<std::result::Result<String, String>> {
        let component = self
            .handlers
            .get(name)
            .ok_or_else(|| anyhow!("no handler named `{name}` (pass --load {name}=PATH)"))?;

        let mut linker = Linker::<HandlerStoreState>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdio();
        for (k, v) in &self.env {
            builder.env(k, v);
        }
        let mut store = Store::new(
            &self.engine,
            HandlerStoreState {
                table: ResourceTable::new(),
                wasi: builder.build(),
            },
        );

        // Instantiate through the wasmtime linker directly — bindgen's
        // `RequestHandlerPre` wrapper only added the typed-export
        // accessor; the underlying pre/instance shape is untouched.
        let instance_pre = linker.instantiate_pre(component)?;
        let instance = instance_pre.instantiate(store.as_context_mut())?;

        // Call `duckdb:handler/handler.handle(request: string) ->
        // result<string, string>` through the sync export bridge.
        // Interface name matches the WIT declaration verbatim (see
        // `wit/handler/handler.wit` — `package duckdb:handler` +
        // `interface handler`). Version tags matter to wasmtime's
        // export lookup; the world here declares no `@x.y.z` so the
        // qualified name is the bare `duckdb:handler/handler`.
        let ret = sync_export_bridge::call_export(
            store.as_context_mut(),
            &instance,
            Some("duckdb:handler/handler"),
            "handle",
            &[Value::String(request_json.to_string())],
        )
        .map_err(|e| anyhow!("handler `{name}` call: {e}"))?;

        // Unpack `result<string, string>` — the bridge lifts a WIT
        // result to `Value::Result(Result<Option<Box<Value>>,
        // Option<Box<Value>>>)`. Both arms of our result carry a
        // string payload, so both `Ok(Some(_))` and `Err(Some(_))`
        // are the expected shapes; a `None` inner (which would
        // correspond to WIT `result` / `result<_, E>` / `result<T>`
        // with an absent payload) is a contract violation for this
        // signature.
        match ret.as_slice() {
            [Value::Result(inner)] => match inner {
                Ok(Some(payload)) => match payload.as_ref() {
                    Value::String(s) => Ok(Ok(s.clone())),
                    other => Err(anyhow!(
                        "handler `{name}`: expected Value::String in \
                         Ok payload, got {other:?}"
                    )),
                },
                Err(Some(payload)) => match payload.as_ref() {
                    Value::String(s) => Ok(Err(s.clone())),
                    other => Err(anyhow!(
                        "handler `{name}`: expected Value::String in \
                         Err payload, got {other:?}"
                    )),
                },
                Ok(None) | Err(None) => Err(anyhow!(
                    "handler `{name}`: result<string, string> payload \
                     was None — contract violation"
                )),
            },
            other => Err(anyhow!(
                "handler `{name}`: expected exactly one Value::Result \
                 return, got {other:?}"
            )),
        }
    }
}
