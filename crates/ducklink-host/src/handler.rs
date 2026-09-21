//! Request-handler component support for duckdb-wasm-httpd.
//!
//! Implements the host side of the `duckdb:handler/request-handler` world:
//! load wasm components (`--load NAME=PATH`) that export
//! `handler.handle(request: string) -> result<string, string>`, and invoke
//! one per HTTP request whose route has `kind='wasm'`. Mirrors sqlite-wasm's
//! `language-runtime` dispatcher: each call gets a FRESH instance, so
//! handlers are stateless across requests (persistent state belongs in the DB).
//!
//! ## Migration (Path B, ADR-0029)
//!
//! Under Path A this file dispatched guest exports through the
//! escape-hatch `sync_export_bridge` on a caller-owned
//! `wasmtime::Linker` + `Store`. Under Path B (2026-09-21) both are
//! gone — components compile through
//! `wasmos_runtime_wasmtime_v48::SyncRuntime::compile_component` and
//! instances come out of `SyncRuntime::instantiate` as opaque
//! `SyncInstance` handles. Exports dispatch through
//! `SyncInstance::call_export`. Zero direct wasmtime types in this
//! file; the `SyncRuntime`'s private tokio runtime handles the
//! wasmos-native async cascade internally.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use wasmos_runtime_api::{
    CompileOptions, CompiledComponent, ComponentSource, ExecutionContext, RuntimeConfig, Value,
    WasiEnvironment,
};
use wasmos_runtime_wasmtime_v48::SyncRuntime;

/// Loaded request-handler components, keyed by the name given to `--load`.
pub struct HandlerRegistry {
    sync_rt: SyncRuntime,
    handlers: HashMap<String, CompiledComponent>,
    env: Vec<(String, String)>,
}

impl HandlerRegistry {
    /// Build an empty registry. `env` is the set of env vars forwarded into
    /// every handler invocation (no process env is exposed otherwise).
    pub fn new(env: Vec<(String, String)>) -> Result<Self> {
        let sync_rt = SyncRuntime::new(RuntimeConfig::default())
            .map_err(|e| anyhow!("build SyncRuntime for handler registry: {e}"))?;
        Ok(Self {
            sync_rt,
            handlers: HashMap::new(),
            env,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Compile + register a handler component under `name`.
    pub fn register(&mut self, name: &str, path: &Path) -> Result<()> {
        let compiled = self
            .sync_rt
            .compile_component(
                ComponentSource::Path {
                    path: path.to_path_buf(),
                    name: Some(name.to_string()),
                },
                CompileOptions::default(),
            )
            .map_err(|e| anyhow!("load handler component {}: {e}", path.display()))?;
        self.handlers.insert(name.to_string(), compiled);
        Ok(())
    }

    /// Invoke the named handler with `request_json`. Returns the handler's
    /// `Ok(body)` / `Err(message)`. A fresh instance per call keeps
    /// handlers stateless across requests.
    pub fn invoke(
        &self,
        name: &str,
        request_json: &str,
    ) -> Result<std::result::Result<String, String>> {
        let compiled = self
            .handlers
            .get(name)
            .ok_or_else(|| anyhow!("no handler named `{name}` (pass --load {name}=PATH)"))?;

        let wasi_env = self
            .env
            .iter()
            .fold(WasiEnvironment::inherit_stdio(), |env, (k, v)| {
                env.with_env(k, v)
            });

        let ctx = ExecutionContext::new().with_wasi(wasi_env);
        let mut instance = self
            .sync_rt
            .instantiate(compiled, ctx)
            .map_err(|e| anyhow!("handler `{name}` instantiate: {e}"))?;

        // Call `duckdb:handler/handler.handle(request: string) ->
        // result<string, string>`. Interface name matches the WIT
        // declaration verbatim (see `wit/handler/handler.wit` —
        // `package duckdb:handler` + `interface handler`). The world
        // here declares no `@x.y.z` so the qualified name is the
        // bare `duckdb:handler/handler`.
        let ret = instance
            .call_export(
                "duckdb:handler/handler#handle",
                &[Value::String(request_json.to_string())],
            )
            .map_err(|e| anyhow!("handler `{name}` call: {e}"))?;

        // Unpack `result<string, string>` — the adapter lifts a WIT
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
