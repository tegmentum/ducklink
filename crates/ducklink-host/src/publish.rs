//! `ducklink publish` — publish the catalog + content-addressed artifacts to the
//! shared Cloudflare R2 extension-distribution bucket (`datalink-ext`).
//!
//! This is the upload side of the final distribution design (see the
//! `extension-distribution-r2` memory): R2 is the artifact CDN, content-addressed
//! + immutable (so Cloudflare's edge caches forever and the origin sees ~0 reads,
//! and R2 charges $0 egress). The browser demo + stock-DuckDB `INSTALL FROM`
//! fetch the bytes DIRECTLY from R2; the host is never in the byte path.
//!
//! SINGLE shared bucket layout (holds BOTH ducklink + sqlink; this tool only
//! writes the ducklink side, reserving the sqlink namespace):
//!
//! ```text
//!   wasm/sha256/<digest>/<name>.wasm          content-addressed shared store
//!                                             (DB-agnostic providers + per-DB
//!                                              exts; dedup by digest, immutable)
//!   ducklink/catalog.json                     the per-DB catalog (short TTL)
//!   sqlink/catalog.json                       RESERVED (sqlink publishes its own)
//!   ${REVISION}/${PLATFORM}/<name>.duckdb_extension(.gz)
//!                                             the stock-DuckDB
//!                                             custom_extension_repository tree
//!                                             (emitted by gen-shim.sh), immutable
//! ```
//!
//! Transport: native blocking `reqwest` over HTTPS to the R2 S3 endpoint
//! `https://<account>.r2.cloudflarestorage.com/<bucket>/<key>`, SigV4-signed
//! (service `s3`, region `auto`) with the SAME [`crate::sigv4`] primitives the
//! duckstream replicator uses. The signed `payload_hash` is the object's content
//! digest (the sha256 of the bytes being PUT) — which, for the content-addressed
//! `wasm/sha256/<digest>` objects, IS the `<digest>` in the key.
//!
//! `--dry-run` plans + prints the object keys/sizes WITHOUT any network or
//! credentials, so the layout can be verified offline (e.g. in this build, since
//! the R2 secrets live only in CI). The live first publish runs from the
//! publish-r2 CI workflow after merge.
//!
//! PREBUILT INTEGRITY GATE: the heavy C-lib extension components (spatialfns /
//! avrofns / azfs / mysqlwasm / postgreswasm / sqlitewasm …) are NOT recompiled in
//! CI — they are downloaded prebuilt and pinned by the catalog `content_digest`.
//! Set `DUCKLINK_PUBLISH_VERIFY=1` to make BOTH the dry-run plan and the live
//! publish hash every content-addressed artifact and reject any mismatch against
//! the catalog pin (the integrity gate that replaces rebuild-from-source). The
//! live upload path additionally re-verifies each object's bytes immediately
//! before the PUT (see [`object_bytes`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::sigv4::{self, Credentials};

/// `Cache-Control` for content-addressed, never-changing objects (the wasm store
/// + the per-revision native tree): cache for a year, immutable.
pub const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `Cache-Control` for the mutable catalog pointer: a short TTL so a republish is
/// visible quickly (the catalog points at the immutable store).
pub const CACHE_CATALOG: &str = "public, max-age=60, must-revalidate";

/// Where an object's bytes come from when uploading.
#[derive(Clone, Debug)]
pub enum ObjectSource {
    /// Read from this local file at upload time.
    File(PathBuf),
    /// In-memory bytes (the catalog JSON).
    Bytes(Vec<u8>),
}

/// One planned R2 object: its bucket-relative key, size, cache policy, and (for
/// content-addressed objects) the expected content digest.
#[derive(Clone, Debug)]
pub struct PlannedObject {
    pub key: String,
    pub size: u64,
    pub cache_control: &'static str,
    /// The content digest (sha256 hex) where the object is content-addressed.
    pub digest: Option<String>,
    pub source: ObjectSource,
}

/// The full publish plan: the objects to upload + how many duplicates the
/// content-addressing collapsed (the cross-DB / repeated-digest dedup win).
#[derive(Clone, Debug, Default)]
pub struct PublishPlan {
    pub objects: Vec<PlannedObject>,
    pub deduped: usize,
}

impl PublishPlan {
    pub fn total_bytes(&self) -> u64 {
        self.objects.iter().map(|o| o.size).sum()
    }
}

/// R2 connection config, resolved from the environment (the org-level GitHub
/// secrets in CI): `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `R2_ACCOUNT_ID`,
/// `R2_BUCKET`. The optional public hostname (`DUCKLINK_R2_PUBLIC_HOST`) is for
/// the read-serving repoint URLs and is NOT needed to upload.
pub struct R2Config {
    creds: Credentials,
    account_id: String,
    bucket: String,
}

impl R2Config {
    pub fn from_env() -> Result<Self> {
        let access_key = req_env("R2_ACCESS_KEY_ID")?;
        let secret_key = req_env("R2_SECRET_ACCESS_KEY")?;
        let account_id = req_env("R2_ACCOUNT_ID")?;
        let bucket = req_env("R2_BUCKET")?;
        Ok(Self {
            creds: Credentials {
                access_key,
                secret_key,
                session_token: None,
            },
            account_id,
            bucket,
        })
    }

    /// The R2 S3-compatible endpoint origin (no scheme stripped): always HTTPS.
    fn endpoint_host(&self) -> String {
        format!("{}.r2.cloudflarestorage.com", self.account_id)
    }
}

fn req_env(key: &str) -> Result<String> {
    std::env::var(key)
        .map_err(|_| anyhow!("{key} not set (the R2 publish credentials live in CI org secrets)"))
        .and_then(|v| {
            if v.trim().is_empty() {
                bail!("{key} is empty")
            } else {
                Ok(v)
            }
        })
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// Inputs for building a publish plan. `bucket`/`account`/`public_host` are kept
/// out of plan building (they affect only the live upload + the repoint URLs),
/// so the plan is identical in dry-run and live runs.
pub struct PlanInputs<'a> {
    /// The catalog JSON file to publish (e.g. `registry/index.json`).
    pub catalog_path: &'a Path,
    /// Directory of built `.wasm` components (e.g. `artifacts/extensions`); used
    /// to size each artifact when the catalog `artifact` path is relative.
    pub artifacts_dir: &'a Path,
    /// Optional gen-shim output tree (`<repo>/<rev>/<platform>/<name>.duckdb_extension(.gz)`)
    /// to mirror into the stock-DuckDB `custom_extension_repository` layout.
    pub native_repo: Option<&'a Path>,
    /// The per-DB catalog namespace prefix (`ducklink`); `sqlink` is reserved.
    pub db: &'a str,
}

/// Build the publish plan from the catalog + artifacts, content-addressing the
/// wasm store and deduping repeated digests. Pure + offline (no network/creds),
/// so `--dry-run` and the live publish share it exactly.
pub fn plan_publish(inputs: &PlanInputs) -> Result<PublishPlan> {
    let catalog_bytes = std::fs::read(inputs.catalog_path)
        .with_context(|| format!("read catalog {}", inputs.catalog_path.display()))?;
    let catalog: Value = serde_json::from_slice(&catalog_bytes)
        .with_context(|| format!("parse catalog {}", inputs.catalog_path.display()))?;

    let mut objects: Vec<PlannedObject> = Vec::new();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();
    let mut deduped = 0usize;

    // 1. The content-addressed wasm store: one object per (digest, name). Walk
    //    every extension entry's wasm artifacts (the providers[] shape + the
    //    backward-compat single-artifact shape). Identical digests collapse to
    //    one object (the cross-DB / shared-provider dedup).
    let exts = catalog
        .get("extensions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for e in &exts {
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        for (digest, artifact) in wasm_artifacts(e) {
            if digest.is_empty() {
                bail!("extension `{name}`: wasm artifact has no content_digest (run gen-catalog first)");
            }
            let key = format!("wasm/sha256/{digest}/{name}.wasm");
            if !seen_keys.insert(key.clone()) {
                deduped += 1;
                continue;
            }
            let path = resolve_artifact_path(inputs, &artifact, name);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            objects.push(PlannedObject {
                key,
                size,
                cache_control: CACHE_IMMUTABLE,
                digest: Some(digest),
                source: ObjectSource::File(path),
            });
        }
    }

    // PREBUILT INTEGRITY GATE: when DUCKLINK_PUBLISH_VERIFY is truthy, hash every
    // content-addressed wasm object NOW and reject any mismatch (or missing file).
    // This is the gate that REPLACES rebuild-from-source in the publish-r2 CI: the
    // heavy C-lib components are downloaded prebuilt (not recompiled), so the only
    // thing standing between a prebuilt artifact and the CDN is its sha256 ==
    // catalog content_digest. Env-gated so the live publish + the CI dry-run both
    // verify, while the offline unit tests (fixture digests) stay fast + pure.
    if verify_enabled() {
        verify_objects(&objects)?;
    }

    // 2. The per-DB catalog (mutable, short-TTL), pointing at the wasm store.
    //    `sqlink/catalog.json` is RESERVED in the layout (sqlink publishes its
    //    own to the shared bucket) — we deliberately do NOT write it here.
    objects.push(PlannedObject {
        key: format!("{}/catalog.json", inputs.db),
        size: catalog_bytes.len() as u64,
        cache_control: CACHE_CATALOG,
        digest: None,
        source: ObjectSource::Bytes(catalog_bytes),
    });

    // 3. The stock-DuckDB custom_extension_repository tree (gen-shim output):
    //    ${REVISION}/${PLATFORM}/<name>.duckdb_extension(.gz), immutable.
    if let Some(repo) = inputs.native_repo {
        for (key, path) in scan_native_repo(repo)? {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            objects.push(PlannedObject {
                key,
                size,
                cache_control: CACHE_IMMUTABLE,
                digest: None,
                source: ObjectSource::File(path),
            });
        }
    }

    Ok(PublishPlan { objects, deduped })
}

/// Is the prebuilt content-digest gate enabled? (`DUCKLINK_PUBLISH_VERIFY` set to
/// a truthy value — `1`/`true`/`yes`, case-insensitive). Off by default so the
/// offline plan/dry-run stays pure unless a caller (the CI publish job) opts in.
fn verify_enabled() -> bool {
    matches!(
        std::env::var("DUCKLINK_PUBLISH_VERIFY")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Hash every content-addressed (digest-bearing, file-sourced) object and reject
/// the whole plan on the first mismatch or unreadable file — the prebuilt-artifact
/// integrity gate. Memory-sourced objects (the catalog JSON) carry no digest and
/// are skipped here.
fn verify_objects(objects: &[PlannedObject]) -> Result<()> {
    let mut checked = 0usize;
    for o in objects {
        let Some(expected) = o.digest.as_deref() else {
            continue;
        };
        if expected.is_empty() {
            continue;
        }
        let ObjectSource::File(path) = &o.source else {
            continue;
        };
        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "verify {}: read prebuilt artifact {}",
                o.key,
                path.display()
            )
        })?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != expected {
            bail!(
                "prebuilt content_digest mismatch for {} ({}):\n  expected {expected}\n  actual   {actual}\n  \
                 (the prebuilt artifact does not match the catalog pin — rebuild + re-upload the prebuilt-components release)",
                o.key,
                path.display(),
            );
        }
        checked += 1;
    }
    eprintln!("ducklink publish: prebuilt integrity gate OK — {checked} artifact digest(s) verified against the catalog");
    Ok(())
}

/// Pull (content_digest, artifact-path) pairs for every wasm provider of a
/// catalog entry — the explicit `providers[]` shape and the backward-compat
/// single-artifact shape.
fn wasm_artifacts(entry: &Value) -> Vec<(String, String)> {
    let digest_of = |v: &Value| {
        v.get("content_digest")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string()
    };
    let artifact_of = |v: &Value| {
        v.get("artifact")
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_string()
    };
    if let Some(providers) = entry.get("providers").and_then(|v| v.as_array()) {
        providers
            .iter()
            .filter(|p| p.get("kind").and_then(|k| k.as_str()).unwrap_or("wasm") == "wasm")
            .map(|p| (digest_of(p), artifact_of(p)))
            .filter(|(_, a)| !a.is_empty())
            .collect()
    } else if entry.get("artifact").is_some() {
        vec![(digest_of(entry), artifact_of(entry))]
    } else {
        Vec::new()
    }
}

/// Resolve a catalog `artifact` value to a local file for sizing/upload. An
/// absolute path is used as-is; a relative path is tried against the catalog's
/// repo root (the catalog's parent's parent, since the catalog is `registry/`),
/// falling back to `<artifacts_dir>/<name>.wasm`.
fn resolve_artifact_path(inputs: &PlanInputs, artifact: &str, name: &str) -> PathBuf {
    let p = PathBuf::from(artifact);
    if p.is_absolute() {
        return p;
    }
    // The catalog lives at <root>/registry/index.json; artifact paths in it are
    // relative to <root>.
    if let Some(root) = inputs.catalog_path.parent().and_then(|d| d.parent()) {
        let candidate = root.join(&p);
        if candidate.is_file() {
            return candidate;
        }
    }
    let by_name = inputs.artifacts_dir.join(format!("{name}.wasm"));
    if by_name.is_file() {
        return by_name;
    }
    // Last resort: relative to cwd (matches the dev layout); may not exist in a
    // dry-run, in which case sizing reports 0.
    p
}

/// Scan a gen-shim output repo for the `<rev>/<platform>/<name>.duckdb_extension(.gz)`
/// layout, returning (bucket-key, local-path) pairs. The bucket key mirrors the
/// `${REVISION}/${PLATFORM}/<file>` tree verbatim.
fn scan_native_repo(repo: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let rev_iter = match std::fs::read_dir(repo) {
        Ok(it) => it,
        Err(_) => return Ok(out), // absent tree -> nothing to publish
    };
    for rev_ent in rev_iter.flatten() {
        if !rev_ent.path().is_dir() {
            continue;
        }
        let rev = rev_ent.file_name().to_string_lossy().to_string();
        for plat_ent in std::fs::read_dir(rev_ent.path())?.flatten() {
            if !plat_ent.path().is_dir() {
                continue;
            }
            let plat = plat_ent.file_name().to_string_lossy().to_string();
            for file_ent in std::fs::read_dir(plat_ent.path())?.flatten() {
                let path = file_ent.path();
                let fname = file_ent.file_name().to_string_lossy().to_string();
                if fname.contains(".duckdb_extension") && path.is_file() {
                    out.push((format!("{rev}/{plat}/{fname}"), path));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Dry-run rendering
// ---------------------------------------------------------------------------

/// Print the plan (the exact object keys + sizes that WOULD be uploaded to the
/// `datalink-ext` bucket) without any network or credentials.
pub fn print_dry_run(plan: &PublishPlan, bucket: &str, db: &str) {
    println!("ducklink publish --dry-run (no upload; no credentials read)");
    println!("bucket: {bucket}  (single shared bucket; sqlink/ namespace reserved, not written here)");
    println!();
    println!("{:<58}  {:>10}  cache", "key", "size");
    println!("{}", "-".repeat(82));
    for o in &plan.objects {
        let ttl = if o.cache_control == CACHE_IMMUTABLE {
            "immutable"
        } else {
            "short-ttl"
        };
        println!("{:<58}  {:>10}  {ttl}", o.key, human_size(o.size));
    }
    println!("{}", "-".repeat(82));
    println!(
        "{} objects, {} total ({} duplicate digest(s) deduped); catalog -> {db}/catalog.json",
        plan.objects.len(),
        human_size(plan.total_bytes()),
        plan.deduped,
    );
    println!("RESERVED (not written by ducklink): sqlink/catalog.json");
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Live upload (R2 S3 API, SigV4)
// ---------------------------------------------------------------------------

/// A minimal blocking R2 client: path-style addressing to the account endpoint,
/// SigV4-signed PUT (service `s3`, region `auto`). Mirrors the duckstream
/// `S3Client` but pinned to the R2 endpoint + bucket.
struct R2Client {
    cfg: R2Config,
    host_header: String,
    http: reqwest::blocking::Client,
}

impl R2Client {
    fn new(cfg: R2Config) -> Result<Self> {
        let host_header = cfg.endpoint_host();
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            cfg,
            host_header,
            http,
        })
    }

    /// Path-style object URL: `https://<account>.r2.cloudflarestorage.com/<bucket>/<key>`.
    fn object_url(&self, key: &str) -> String {
        format!(
            "https://{}/{}/{}",
            self.host_header,
            self.cfg.bucket,
            sigv4::uri_encode_path(key)
        )
    }

    fn canonical_uri(&self, key: &str) -> String {
        format!("/{}/{}", self.cfg.bucket, sigv4::uri_encode_path(key))
    }

    /// PUT one object. `payload_hash` is the sha256 of the body (the object's
    /// content digest); region is `auto`, service `s3` — the same SigV4 signer
    /// the replicator uses. `Cache-Control` rides as an unsigned header.
    fn put_object(&self, key: &str, body: Vec<u8>, cache_control: &str) -> Result<()> {
        let payload_hash = hex::encode(Sha256::digest(&body));
        let amz_date = sigv4::amz_date_now();
        let headers = vec![
            ("host".to_string(), self.host_header.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let authz = sigv4::sign_v4(
            &self.cfg.creds,
            "PUT",
            "auto",
            "s3",
            &self.canonical_uri(key),
            "",
            &headers,
            &payload_hash,
            &amz_date,
        );
        let mut req = self
            .http
            .put(self.object_url(key))
            .header("cache-control", cache_control);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req = req.header("authorization", authz);
        let resp = req.body(body).send().with_context(|| format!("PUT {key}"))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().unwrap_or_default();
            bail!("PUT {key} failed: HTTP {status}: {txt}");
        }
        Ok(())
    }
}

/// Read a planned object's bytes (from disk or memory), verifying the content
/// digest for content-addressed objects (a tamper/skew guard before upload).
fn object_bytes(o: &PlannedObject) -> Result<Vec<u8>> {
    let bytes = match &o.source {
        ObjectSource::Bytes(b) => b.clone(),
        ObjectSource::File(p) => {
            std::fs::read(p).with_context(|| format!("read {}", p.display()))?
        }
    };
    if let Some(expected) = &o.digest {
        if !expected.is_empty() {
            let actual = hex::encode(Sha256::digest(&bytes));
            if &actual != expected {
                bail!(
                    "content_digest mismatch for {}:\n  expected {expected}\n  actual   {actual}",
                    o.key
                );
            }
        }
    }
    Ok(bytes)
}

/// Run a live publish: plan, then PUT every object to R2. Skips objects already
/// present (content-addressed -> identical bytes -> idempotent) is left to R2;
/// we PUT unconditionally (cheap; immutable keys are stable).
pub fn run_publish(inputs: &PlanInputs) -> Result<()> {
    let plan = plan_publish(inputs)?;
    let cfg = R2Config::from_env()?;
    let bucket = cfg.bucket.clone();
    let client = R2Client::new(cfg)?;
    eprintln!(
        "ducklink publish: {} objects -> r2://{} (endpoint {})",
        plan.objects.len(),
        bucket,
        client.host_header,
    );
    for o in &plan.objects {
        let bytes = object_bytes(o)?;
        client.put_object(&o.key, bytes, o.cache_control)?;
        eprintln!("  PUT {} ({})", o.key, human_size(o.size));
    }
    eprintln!(
        "ducklink publish: done — {} objects, {} total ({} deduped)",
        plan.objects.len(),
        human_size(plan.total_bytes()),
        plan.deduped,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_catalog(dir: &Path, json: &str) -> PathBuf {
        let reg = dir.join("registry");
        std::fs::create_dir_all(&reg).unwrap();
        let p = reg.join("index.json");
        std::fs::write(&p, json).unwrap();
        p
    }

    #[test]
    fn plans_the_datalink_ext_layout() {
        let tmp = tempfile::tempdir().unwrap();
        // Two extensions; `bbb` and `ccc` share a digest+name?? No — share a
        // digest is only deduped when the KEY (digest+name) matches. Use the same
        // shared provider listed twice to exercise dedup.
        let catalog = r#"{
          "extensions": [
            { "name": "aaa", "artifact": "artifacts/extensions/aaa.wasm",
              "content_digest": "1111111111111111111111111111111111111111111111111111111111111111" },
            { "name": "bbb", "providers": [
              { "id": "wasm-component", "kind": "wasm",
                "artifact": "artifacts/extensions/bbb.wasm",
                "content_digest": "2222222222222222222222222222222222222222222222222222222222222222" },
              { "id": "wasm-dup", "kind": "wasm",
                "artifact": "artifacts/extensions/bbb.wasm",
                "content_digest": "2222222222222222222222222222222222222222222222222222222222222222" }
            ] }
          ]
        }"#;
        let catalog_path = write_catalog(tmp.path(), catalog);
        let artifacts_dir = tmp.path().join("artifacts/extensions");
        std::fs::create_dir_all(&artifacts_dir).unwrap();
        for n in ["aaa", "bbb"] {
            let mut f = std::fs::File::create(artifacts_dir.join(format!("{n}.wasm"))).unwrap();
            f.write_all(b"\0asm\x01\0\0\0").unwrap();
        }

        let inputs = PlanInputs {
            catalog_path: &catalog_path,
            artifacts_dir: &artifacts_dir,
            native_repo: None,
            db: "ducklink",
        };
        let plan = plan_publish(&inputs).unwrap();
        let keys: Vec<&str> = plan.objects.iter().map(|o| o.key.as_str()).collect();

        // content-addressed wasm store keys
        assert!(keys.contains(
            &"wasm/sha256/1111111111111111111111111111111111111111111111111111111111111111/aaa.wasm"
        ));
        assert!(keys.contains(
            &"wasm/sha256/2222222222222222222222222222222222222222222222222222222222222222/bbb.wasm"
        ));
        // the per-DB catalog
        assert!(keys.contains(&"ducklink/catalog.json"));
        // the duplicate provider (same digest+name) was deduped
        assert_eq!(plan.deduped, 1);
        // the catalog object carries the short TTL; the wasm store is immutable
        let cat = plan.objects.iter().find(|o| o.key == "ducklink/catalog.json").unwrap();
        assert_eq!(cat.cache_control, CACHE_CATALOG);
        let wasm = plan
            .objects
            .iter()
            .find(|o| o.key.starts_with("wasm/sha256/1111"))
            .unwrap();
        assert_eq!(wasm.cache_control, CACHE_IMMUTABLE);
    }

    #[test]
    fn includes_the_revision_platform_native_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = r#"{ "extensions": [] }"#;
        let catalog_path = write_catalog(tmp.path(), catalog);
        let artifacts_dir = tmp.path().join("artifacts/extensions");
        std::fs::create_dir_all(&artifacts_dir).unwrap();

        // gen-shim layout: <repo>/<rev>/<platform>/<name>.duckdb_extension(.gz)
        let repo = tmp.path().join("native-repo");
        let leaf = repo.join("v1.5.4/osx_arm64");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("aba.duckdb_extension"), b"shim").unwrap();
        std::fs::write(leaf.join("aba.duckdb_extension.gz"), b"gz").unwrap();

        let inputs = PlanInputs {
            catalog_path: &catalog_path,
            artifacts_dir: &artifacts_dir,
            native_repo: Some(&repo),
            db: "ducklink",
        };
        let plan = plan_publish(&inputs).unwrap();
        let keys: Vec<&str> = plan.objects.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"v1.5.4/osx_arm64/aba.duckdb_extension"));
        assert!(keys.contains(&"v1.5.4/osx_arm64/aba.duckdb_extension.gz"));
    }

    #[test]
    fn endpoint_host_is_the_account_r2_domain() {
        let cfg = R2Config {
            creds: Credentials {
                access_key: "k".into(),
                secret_key: "s".into(),
                session_token: None,
            },
            account_id: "a633389b157fd8a9ec3d3a27cd375643".into(),
            bucket: "datalink-ext".into(),
        };
        assert_eq!(
            cfg.endpoint_host(),
            "a633389b157fd8a9ec3d3a27cd375643.r2.cloudflarestorage.com"
        );
    }

    #[test]
    fn verify_objects_rejects_prebuilt_digest_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("x.wasm");
        std::fs::write(&art, b"\0asm\x01\0\0\0").unwrap();
        // A digest that does NOT match sha256(b"\0asm\x01\0\0\0").
        let objs = vec![PlannedObject {
            key: "wasm/sha256/abcd/x.wasm".into(),
            size: 8,
            cache_control: CACHE_IMMUTABLE,
            digest: Some("00".repeat(32)),
            source: ObjectSource::File(art.clone()),
        }];
        let err = verify_objects(&objs).unwrap_err();
        assert!(err.to_string().contains("prebuilt content_digest mismatch"));

        // The matching digest passes.
        let good = hex::encode(Sha256::digest(b"\0asm\x01\0\0\0"));
        let objs = vec![PlannedObject {
            digest: Some(good),
            ..objs.into_iter().next().unwrap()
        }];
        verify_objects(&objs).unwrap();
    }

    #[test]
    fn object_bytes_rejects_digest_skew() {
        let o = PlannedObject {
            key: "wasm/sha256/dead/x.wasm".into(),
            size: 4,
            cache_control: CACHE_IMMUTABLE,
            digest: Some("deadbeef".into()),
            source: ObjectSource::Bytes(b"\0asm".to_vec()),
        };
        let err = object_bytes(&o).unwrap_err();
        assert!(err.to_string().contains("content_digest mismatch"));
    }
}
