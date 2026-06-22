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

use anyhow::{anyhow, Context, Result};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{AsContextMut, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::build_engine;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/handler",
        world: "duckdb:handler/request-handler",
    });
}

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
            .with_context(|| format!("load handler component {}", path.display()))?;
        self.handlers.insert(name.to_string(), component);
        Ok(())
    }

    /// Invoke the named handler with `request_json`. Returns the handler's
    /// `Ok(body)` / `Err(message)`. A fresh Store per call keeps handlers
    /// stateless across requests.
    pub fn invoke(&self, name: &str, request_json: &str) -> Result<std::result::Result<String, String>> {
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

        let pre = bindings::RequestHandlerPre::new(linker.instantiate_pre(component)?)?;
        let instance = pre.instantiate(store.as_context_mut())?;
        let result = instance
            .duckdb_handler_handler()
            .call_handle(store.as_context_mut(), request_json)
            .map_err(|e| anyhow!("handler `{name}` trap: {e}"))?;
        Ok(result)
    }
}
