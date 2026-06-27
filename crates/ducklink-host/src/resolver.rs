//! Multi-provider extension resolver — the spine (design A) from
//! PLAN-multi-provider-extensions.md ("The Resolver" + Appendix B).
//!
//! A logical extension has one semantic contract (`wit_contract`) and one or
//! more `providers[]`, each an implementation of that contract on some substrate
//! (wasm / native / remote). This module is the substrate-agnostic policy +
//! candidate-filtering spine over those providers. It is deliberately
//! self-contained (no wasmtime / no engine types) so it lifts cleanly into
//! `datalink` later; the host injects the concrete wasm `load()` via a callback.
//!
//! This pass implements the Wasm kind only. The Wasm `load()` IS Route A's
//! resident `duckdb:extension` dispatch (datalink-dynlink + register-capture +
//! the direct `callback-dispatch` import) — wrapped, not reimplemented. Native
//! and Remote are stubbed variants that are filtered out as unavailable.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// B.2 types
// ---------------------------------------------------------------------------

/// Content-addressed artifact reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentRef {
    Path(PathBuf),
    Digest(String),
    Oci(String),
}

/// Substrate of one provider. Only `Wasm` is implemented this pass; `Native` and
/// `Remote` are scaffolded so the filtering pipeline is real (they resolve as
/// unavailable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    Wasm {
        abi: String,
        artifact: ContentRef,
        content_digest: Option<String>,
        browser_safe: bool,
    },
    Native {
        os: String,
        arch: String,
        artifact: ContentRef,
    },
    Remote {
        endpoint: String,
    },
}

impl ProviderKind {
    pub fn tag(&self) -> &'static str {
        match self {
            ProviderKind::Wasm { .. } => "wasm",
            ProviderKind::Native { .. } => "native",
            ProviderKind::Remote { .. } => "remote",
        }
    }
}

/// The SEMANTIC-contract certificate carried by a provider entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conformance {
    pub suite: String,
    /// Content digest of the suite itself; must equal the canonical suite the
    /// resolver holds for (ext, contract).
    pub suite_digest: String,
    /// Must equal the logical extension's `wit_contract`.
    pub contract_digest: String,
    pub passed: bool,
}

/// Native/remote admission inputs (unused for wasm, which is sandboxed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trust {
    pub signed_by: Option<String>,
    pub attestation: Option<String>,
}

/// Static descriptor parsed from one manifest provider entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub id: String,
    pub kind: ProviderKind,
    /// Defines semantics (the wasm baseline). A reference wasm provider is
    /// certified-by-construction at the entry's contract.
    pub reference: bool,
    pub conformance: Option<Conformance>,
    pub trust: Option<Trust>,
}

/// One logical extension entry: identity + semantic contract + providers.
#[derive(Clone, Debug)]
pub struct ManifestEntry {
    pub name: String,
    /// THE semantic_contract digest (witcanon).
    pub wit_contract: String,
    pub providers: Vec<ProviderDescriptor>,
}

/// Environment inputs to availability/precedence.
#[derive(Clone, Debug)]
pub struct Env {
    /// A wasm runtime is present in-process (always true for the ducklink host).
    pub wasm_runtime: bool,
    /// Native `.duckdb_extension` loading allowed (this pass: always false).
    pub allow_native: bool,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            wasm_runtime: true,
            allow_native: false,
        }
    }
}

/// Resolution policy (the overridable knobs from the doc).
#[derive(Clone, Debug, Default)]
pub struct ResolvePolicy {
    /// `SET extension_provider = '<id>'` — force a specific provider id.
    pub forced_provider: Option<String>,
    /// `SET extension_provider_deny = '<id>,<id>'` — user-excluded providers.
    pub denied: Vec<String>,
}

// ---------------------------------------------------------------------------
// Candidate pipeline outcome (observability)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Chosen,
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateOutcome {
    pub id: String,
    pub kind: &'static str,
    pub outcome: Outcome,
}

#[derive(Clone, Debug)]
pub struct Resolution {
    pub extension: String,
    pub chosen_id: String,
    pub chosen_kind: &'static str,
    /// The resolved artifact of the chosen provider (for the substrate loader).
    pub artifact: ContentRef,
    /// Per-candidate outcome, in evaluation order (the chosen + why each loser lost).
    pub reasoning: Vec<CandidateOutcome>,
}

#[derive(Clone, Debug)]
pub struct ResolveError {
    pub extension: String,
    pub reasoning: Vec<CandidateOutcome>,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no admissible provider for '{}': {}",
            self.extension,
            render_reasoning(&self.reasoning)
        )
    }
}

impl std::error::Error for ResolveError {}

/// Render per-candidate reasoning as a single line (used by the error + the
/// `extension_provider` observability function).
pub fn render_reasoning(reasoning: &[CandidateOutcome]) -> String {
    reasoning
        .iter()
        .map(|c| match &c.outcome {
            Outcome::Chosen => format!("{} [{}] = CHOSEN", c.id, c.kind),
            Outcome::Rejected(why) => format!("{} [{}] = rejected ({})", c.id, c.kind, why),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

/// Precedence rank (lower = preferred): native(trusted) > wasm-local >
/// wasm-browser > remote.
fn precedence_rank(kind: &ProviderKind) -> u8 {
    match kind {
        ProviderKind::Native { .. } => 0,
        ProviderKind::Wasm { browser_safe: false, .. } => 1,
        ProviderKind::Wasm { browser_safe: true, .. } => 2,
        ProviderKind::Remote { .. } => 3,
    }
}

/// The conformance HARD GATE. A provider is certified iff:
///   passed && contract_digest == wit_contract && suite_digest == canonical.
/// A reference wasm provider is certified-by-construction (the baseline WIT *is*
/// the contract that defines the semantics).
///
/// `canonical_suite_digest` is the suite the resolver holds for (ext, contract);
/// `None` means no suite is registered yet, so the suite_digest sub-check is
/// scaffolded (skipped) — the gate still enforces passed + contract match.
fn conformance_ok(
    p: &ProviderDescriptor,
    wit_contract: &str,
    canonical_suite_digest: Option<&str>,
) -> Result<(), String> {
    if p.reference && matches!(p.kind, ProviderKind::Wasm { .. }) {
        return Ok(());
    }
    match &p.conformance {
        None => Err("uncertified: no conformance record".to_string()),
        Some(c) => {
            if !c.passed {
                return Err("uncertified: conformance.passed=false".to_string());
            }
            if c.contract_digest != wit_contract {
                return Err(format!(
                    "uncertified: certified at contract {} != live {}",
                    short(&c.contract_digest),
                    short(wit_contract)
                ));
            }
            if let Some(canon) = canonical_suite_digest {
                if c.suite_digest != canon {
                    return Err(format!(
                        "uncertified: suite_digest {} != canonical {}",
                        short(&c.suite_digest),
                        short(canon)
                    ));
                }
            }
            Ok(())
        }
    }
}

fn substrate_available(kind: &ProviderKind, env: &Env) -> Result<(), String> {
    match kind {
        ProviderKind::Wasm { .. } => {
            if env.wasm_runtime {
                Ok(())
            } else {
                Err("no wasm runtime".to_string())
            }
        }
        ProviderKind::Native { os, arch, .. } => {
            // Stub: native loading not implemented this pass.
            if !env.allow_native {
                Err("native providers disabled (allow_native=false)".to_string())
            } else {
                Err(format!("native loader not implemented ({os}/{arch})"))
            }
        }
        ProviderKind::Remote { endpoint } => {
            Err(format!("remote loader not implemented ({endpoint})"))
        }
    }
}

fn trusted(kind: &ProviderKind) -> Result<(), String> {
    match kind {
        // wasm is sandboxed -> always trusted.
        ProviderKind::Wasm { .. } => Ok(()),
        ProviderKind::Native { .. } => Err("native trust policy not implemented".to_string()),
        ProviderKind::Remote { .. } => Err("remote trust policy not implemented".to_string()),
    }
}

/// Run the candidate pipeline and pick a provider, or fail with per-candidate
/// reasons. The pipeline order mirrors the doc:
///   conformance gate -> available -> trusted -> !user_excluded -> !forced_out
///   -> order by precedence -> first.
pub fn resolve(
    entry: &ManifestEntry,
    env: &Env,
    policy: &ResolvePolicy,
    canonical_suite_digest: Option<&str>,
) -> Result<Resolution, ResolveError> {
    let mut reasoning: Vec<CandidateOutcome> = Vec::new();
    let mut admitted: Vec<&ProviderDescriptor> = Vec::new();

    for p in &entry.providers {
        let kind = p.kind.tag();
        // 1. conformance HARD GATE
        if let Err(why) = conformance_ok(p, &entry.wit_contract, canonical_suite_digest) {
            reasoning.push(reject(&p.id, kind, why));
            continue;
        }
        // 2. substrate available
        if let Err(why) = substrate_available(&p.kind, env) {
            reasoning.push(reject(&p.id, kind, why));
            continue;
        }
        // 3. trusted
        if let Err(why) = trusted(&p.kind) {
            reasoning.push(reject(&p.id, kind, why));
            continue;
        }
        // 4. not user-excluded
        if policy.denied.iter().any(|d| d == &p.id) {
            reasoning.push(reject(&p.id, kind, "user-excluded (deny)".to_string()));
            continue;
        }
        // 5. forced-provider override: if set, only that id survives
        if let Some(forced) = &policy.forced_provider {
            if forced != &p.id {
                reasoning.push(reject(
                    &p.id,
                    kind,
                    format!("not the forced provider ('{forced}')"),
                ));
                continue;
            }
        }
        admitted.push(p);
    }

    // 6. order by precedence (stable; manifest order breaks ties)
    admitted.sort_by_key(|p| precedence_rank(&p.kind));

    match admitted.first() {
        Some(chosen) => {
            // Record the chosen + keep the losers' reasons already collected.
            reasoning.insert(
                0,
                CandidateOutcome {
                    id: chosen.id.clone(),
                    kind: chosen.kind.tag(),
                    outcome: Outcome::Chosen,
                },
            );
            // Any other admitted providers lost on precedence.
            for other in admitted.iter().skip(1) {
                reasoning.push(reject(
                    &other.id,
                    other.kind.tag(),
                    "lost on precedence".to_string(),
                ));
            }
            let artifact = match &chosen.kind {
                ProviderKind::Wasm { artifact, .. } => artifact.clone(),
                ProviderKind::Native { artifact, .. } => artifact.clone(),
                ProviderKind::Remote { endpoint } => ContentRef::Oci(endpoint.clone()),
            };
            Ok(Resolution {
                extension: entry.name.clone(),
                chosen_id: chosen.id.clone(),
                chosen_kind: chosen.kind.tag(),
                artifact,
                reasoning,
            })
        }
        None => Err(ResolveError {
            extension: entry.name.clone(),
            reasoning,
        }),
    }
}

fn reject(id: &str, kind: &'static str, why: String) -> CandidateOutcome {
    CandidateOutcome {
        id: id.to_string(),
        kind,
        outcome: Outcome::Rejected(why),
    }
}

fn short(digest: &str) -> String {
    digest.chars().take(12).collect()
}

// ---------------------------------------------------------------------------
// B.1 manifest reader (providers[] + backward-compat single-artifact)
// ---------------------------------------------------------------------------

/// Read one extension's manifest entry from a parsed `registry/index.json`
/// value. Returns `None` if the extension is absent or has no usable artifact.
///
/// Backward compatibility: a current single-artifact entry (just `artifact` +
/// `content_digest` + `wit_contract`) is read as a one-element `providers[]` with
/// `kind:"wasm", reference:true`, lifting those fields verbatim. An explicit
/// `providers[]` array (the generalized shape) is read directly.
pub fn read_manifest_entry(index: &serde_json::Value, name: &str) -> Option<ManifestEntry> {
    let exts = index.get("extensions")?.as_array()?;
    let entry = exts
        .iter()
        .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))?;

    let wit_contract = entry
        .get("wit_contract")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let abi = format!(
        "duckdb:extension@{}",
        entry
            .get("wit_contract_version")
            .and_then(|v| v.as_str())
            .unwrap_or("2.0.0")
    );

    let mut providers = Vec::new();
    if let Some(arr) = entry.get("providers").and_then(|v| v.as_array()) {
        // Generalized shape.
        for p in arr {
            if let Some(desc) = parse_provider(p, &abi) {
                providers.push(desc);
            }
        }
    } else if let Some(artifact) = entry.get("artifact").and_then(|v| v.as_str()) {
        // Backward-compat: single artifact -> one wasm reference provider.
        providers.push(ProviderDescriptor {
            id: "wasm-component".to_string(),
            kind: ProviderKind::Wasm {
                abi: abi.clone(),
                artifact: ContentRef::Path(PathBuf::from(artifact)),
                content_digest: entry
                    .get("content_digest")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                browser_safe: false,
            },
            reference: true,
            conformance: None, // reference wasm: certified-by-construction
            trust: None,
        });
    }

    if providers.is_empty() {
        return None;
    }
    Some(ManifestEntry {
        name: name.to_string(),
        wit_contract,
        providers,
    })
}

fn parse_provider(p: &serde_json::Value, default_abi: &str) -> Option<ProviderDescriptor> {
    let id = p.get("id").and_then(|v| v.as_str())?.to_string();
    let kind_tag = p.get("kind").and_then(|v| v.as_str()).unwrap_or("wasm");
    let reference = p.get("reference").and_then(|v| v.as_bool()).unwrap_or(false);

    let kind = match kind_tag {
        "wasm" => {
            let artifact = p.get("artifact").and_then(|v| v.as_str())?;
            ProviderKind::Wasm {
                abi: p
                    .get("abi")
                    .and_then(|v| v.as_str())
                    .unwrap_or(default_abi)
                    .to_string(),
                artifact: ContentRef::Path(PathBuf::from(artifact)),
                content_digest: p
                    .get("content_digest")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                browser_safe: p
                    .get("browser_safe")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }
        }
        "native" => {
            let plat = p.get("platform");
            ProviderKind::Native {
                os: plat
                    .and_then(|v| v.get("os"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                arch: plat
                    .and_then(|v| v.get("arch"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                artifact: ContentRef::Oci(
                    p.get("artifact")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
            }
        }
        "remote" => ProviderKind::Remote {
            endpoint: p
                .get("endpoint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        _ => return None,
    };

    let conformance = p.get("conformance").map(|c| Conformance {
        suite: c.get("suite").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        suite_digest: c
            .get("suite_digest")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        contract_digest: c.get("at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        passed: c.get("passed").and_then(|v| v.as_bool()).unwrap_or(false),
    });

    let trust = p.get("trust").map(|t| Trust {
        signed_by: t.get("signed_by").and_then(|v| v.as_str()).map(|s| s.to_string()),
        attestation: t
            .get("attestation")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    });

    Some(ProviderDescriptor {
        id,
        kind,
        reference,
        conformance,
        trust,
    })
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn wasm_ref(id: &str) -> ProviderDescriptor {
        ProviderDescriptor {
            id: id.to_string(),
            kind: ProviderKind::Wasm {
                abi: "duckdb:extension@2.0.0".to_string(),
                artifact: ContentRef::Path(PathBuf::from("artifacts/extensions/aba.wasm")),
                content_digest: Some("366cdf".to_string()),
                browser_safe: false,
            },
            reference: true,
            conformance: None,
            trust: None,
        }
    }

    fn entry(providers: Vec<ProviderDescriptor>) -> ManifestEntry {
        ManifestEntry {
            name: "aba".to_string(),
            wit_contract: "90fdc46a585c".to_string(),
            providers,
        }
    }

    #[test]
    fn reference_wasm_is_chosen_certified_by_construction() {
        let r = resolve(&entry(vec![wasm_ref("wasm-component")]), &Env::default(), &ResolvePolicy::default(), None)
            .expect("resolves");
        assert_eq!(r.chosen_id, "wasm-component");
        assert_eq!(r.chosen_kind, "wasm");
        assert!(matches!(r.reasoning[0].outcome, Outcome::Chosen));
    }

    #[test]
    fn forced_provider_excludes_others() {
        let r = resolve(
            &entry(vec![wasm_ref("wasm-component")]),
            &Env::default(),
            &ResolvePolicy { forced_provider: Some("wasm-component".into()), denied: vec![] },
            None,
        )
        .expect("forced match resolves");
        assert_eq!(r.chosen_id, "wasm-component");
    }

    #[test]
    fn forced_unknown_provider_fails_with_reason() {
        let err = resolve(
            &entry(vec![wasm_ref("wasm-component")]),
            &Env::default(),
            &ResolvePolicy { forced_provider: Some("nope".into()), denied: vec![] },
            None,
        )
        .unwrap_err();
        assert!(render_reasoning(&err.reasoning).contains("not the forced provider"));
    }

    #[test]
    fn denied_provider_is_excluded() {
        let err = resolve(
            &entry(vec![wasm_ref("wasm-component")]),
            &Env::default(),
            &ResolvePolicy { forced_provider: None, denied: vec!["wasm-component".into()] },
            None,
        )
        .unwrap_err();
        assert!(render_reasoning(&err.reasoning).contains("user-excluded"));
    }

    #[test]
    fn uncertified_nonreference_provider_is_gated_out() {
        // A non-reference wasm provider with a conformance record at the WRONG
        // contract is rejected by the hard gate.
        let mut p = wasm_ref("wasm-stale");
        p.reference = false;
        p.conformance = Some(Conformance {
            suite: "aba@2".into(),
            suite_digest: "7f3c".into(),
            contract_digest: "DEADBEEF".into(), // != wit_contract
            passed: true,
        });
        let err = resolve(&entry(vec![p]), &Env::default(), &ResolvePolicy::default(), None)
            .unwrap_err();
        assert!(render_reasoning(&err.reasoning).contains("uncertified"));
    }

    #[test]
    fn native_and_remote_are_unavailable_this_pass() {
        let native = ProviderDescriptor {
            id: "native-linux-x86_64".into(),
            kind: ProviderKind::Native { os: "linux".into(), arch: "x86_64".into(), artifact: ContentRef::Oci("oci://x".into()) },
            reference: false,
            conformance: Some(Conformance { suite: "aba@2".into(), suite_digest: "7f3c".into(), contract_digest: "90fdc46a585c".into(), passed: true }),
            trust: None,
        };
        // native is certified but unavailable -> wasm reference wins.
        let r = resolve(&entry(vec![native, wasm_ref("wasm-component")]), &Env::default(), &ResolvePolicy::default(), None).expect("resolves to wasm");
        assert_eq!(r.chosen_id, "wasm-component");
        assert!(render_reasoning(&r.reasoning).contains("native-linux-x86_64"));
    }

    #[test]
    fn backward_compat_single_artifact_reads_as_one_wasm_reference() {
        let index = serde_json::json!({
            "extensions": [
                { "name": "aba", "artifact": "artifacts/extensions/aba.wasm",
                  "content_digest": "366cdf", "wit_contract": "90fdc46a585c",
                  "wit_contract_version": "2.0.0" }
            ]
        });
        let e = read_manifest_entry(&index, "aba").expect("entry");
        assert_eq!(e.providers.len(), 1);
        assert_eq!(e.providers[0].id, "wasm-component");
        assert!(e.providers[0].reference);
        assert_eq!(e.wit_contract, "90fdc46a585c");
    }

    #[test]
    fn explicit_providers_array_is_read() {
        let index = serde_json::json!({
            "extensions": [
                { "name": "aba", "wit_contract": "90fdc46a585c", "wit_contract_version": "2.0.0",
                  "providers": [
                    { "id": "wasm-component", "kind": "wasm", "reference": true,
                      "artifact": "artifacts/extensions/aba.wasm", "content_digest": "366cdf" }
                  ] }
            ]
        });
        let e = read_manifest_entry(&index, "aba").expect("entry");
        assert_eq!(e.providers.len(), 1);
        assert_eq!(e.providers[0].id, "wasm-component");
        assert!(e.providers[0].reference);
    }
}
