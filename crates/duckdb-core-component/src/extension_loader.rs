use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::{Capabilitykind, DuckDbError};

#[derive(Clone)]
pub struct RegistryEntry {
    pub name: String,
    pub requires: Vec<Capabilitykind>,
    pub wasm_path: PathBuf,
}

impl std::fmt::Debug for RegistryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryEntry")
            .field("requires", &self.requires)
            .field("wasm_path", &self.wasm_path)
            .finish()
    }
}

fn registry() -> &'static Mutex<Vec<RegistryEntry>> {
    static REGISTRY: OnceLock<Mutex<Vec<RegistryEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

pub struct ArtifactLocator {
    pub root: PathBuf,
}

impl ArtifactLocator {
    pub fn default() -> Self {
        Self {
            root: PathBuf::from("artifacts/extensions"),
        }
    }

    pub fn resolve(&self, name: &str) -> PathBuf {
        let sanitized = sanitize_name(name);
        self.root.join(format!("{sanitized}.wasm"))
    }
}

fn sanitize_name(name: &str) -> Cow<'_, str> {
    if name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Cow::Borrowed(name);
    }
    let mut normalized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            normalized.push(ch);
        } else {
            normalized.push('_');
        }
    }
    Cow::Owned(normalized)
}

pub fn record_extension_registration(
    name: &str,
    requires: &[Capabilitykind],
) -> Result<(), DuckDbError> {
    let locator = ArtifactLocator::default();
    let wasm_path = locator.resolve(name);
    let capability_summary = summarize_capabilities(requires);
    let artifact_path = wasm_path.display().to_string();
    crate::clog!(
        "[duckdb-core] register_extension metadata request: name='{name}', requires={capability_summary}, artifact={artifact_path}"
    );
    let mut guard = registry()
        .lock()
        .map_err(|_| DuckDbError::message("extension registry poisoned"))?;
    if let Some(existing) = guard.iter_mut().find(|entry| entry.name == name) {
        if existing.requires.as_slice() != requires {
            let previous = summarize_capabilities(&existing.requires);
            crate::clog!(
                "[duckdb-core] updating capabilities for '{name}': {previous} -> {capability_summary}"
            );
        } else {
            crate::clog!(
                "[duckdb-core] '{name}' already registered with identical capabilities; refreshing entry"
            );
        }
        existing.requires = requires.to_vec();
    } else {
        crate::clog!(
            "[duckdb-core] recorded new extension '{name}' with artifact {artifact_path}"
        );
        guard.push(RegistryEntry {
            name: name.to_string(),
            requires: requires.to_vec(),
            wasm_path,
        });
    }
    Ok(())
}

pub fn list_registered_extensions() -> Vec<RegistryEntry> {
    registry()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

fn summarize_capabilities(requires: &[Capabilitykind]) -> String {
    if requires.is_empty() {
        return "none".to_string();
    }
    let mut parts = Vec::with_capacity(requires.len());
    for cap in requires {
        parts.push(describe_capability(*cap));
    }
    parts.join(", ")
}

fn describe_capability(kind: Capabilitykind) -> &'static str {
    match kind {
        Capabilitykind::Scalar => "scalar",
        Capabilitykind::Table => "table",
        Capabilitykind::Aggregate => "aggregate",
        Capabilitykind::Pragma => "pragma",
        Capabilitykind::Macro => "macro",
    }
}
