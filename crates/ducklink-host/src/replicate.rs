//! duckstream: continuous snapshot replication of a persistent DuckDB
//! database to S3, plus restore. The native-host implementation of the
//! feasibility study's recommended Option (c): **checkpoint-snapshot
//! replication**.
//!
//! Inspired by Litestream (Ben Johnson) for SQLite, adapted for DuckDB; named
//! "duckstream" to avoid implying it is Litestream.
//!
//! Why a full-file snapshot per checkpoint (not WAL shipping like Litestream's
//! continuous WAL replication): DuckDB on wasm has no WAL-hook / commit callback, and it
//! TRUNCATES its WAL on every checkpoint, so the "ship the WAL forever" trick
//! does not apply. The MVP therefore drives an explicit `CHECKPOINT` (which
//! produces a clean, self-contained `.duckdb` and truncates the WAL), then
//! lz4-compresses and uploads the whole `.duckdb`. PITR granularity is coarse
//! (per snapshot interval). Fine-grained WAL-tailing PITR is Phase 2 (deferred).
//!
//! This runs entirely in the NATIVE HOST: ducklink-host already owns the
//! `.duckdb` file via its wasi preopen and already drives the live DuckDB
//! connection, so the replicator is a host task, not an in-guest hook.
//!
//! S3 layout under `<prefix>`:
//!   - `snapshots/{ts}.duckdb.lz4`  — one compressed snapshot per interval
//!   - `latest`                     — text pointer to the newest snapshot key
//!   - `state.json`                 — generation, snapshot ts, sizes, db name
//!
//! Transport: native `reqwest` (blocking) over HTTPS, signed with the SigV4
//! primitives reused verbatim from the wasm s3fs transport (`super::sigv4`) —
//! signing is NOT reimplemented here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::component::ResourceAny;

use crate::sigv4::{self, Credentials};
use crate::ui_server::duckerror_message;
use crate::{
    build_engine, build_wasi_ctx_inherit, instantiate_core, ComponentArtifacts, CoreExecution,
    ExtensionManager,
};

/// Parsed `s3://bucket/prefix` destination.
#[derive(Debug, Clone)]
pub struct S3Target {
    pub bucket: String,
    /// Object-key prefix (no leading/trailing slash), may be empty.
    pub prefix: String,
}

impl S3Target {
    /// Parse `s3://bucket[/prefix...]`.
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("s3://")
            .ok_or_else(|| anyhow!("destination must be an s3:// URL, got {url:?}"))?;
        let (bucket, prefix) = match rest.split_once('/') {
            Some((b, p)) => (b, p),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            bail!("s3:// URL has an empty bucket: {url:?}");
        }
        Ok(Self {
            bucket: bucket.to_string(),
            prefix: prefix.trim_end_matches('/').to_string(),
        })
    }

    /// Join the prefix with a sub-key, yielding the full object key.
    fn key(&self, sub: &str) -> String {
        if self.prefix.is_empty() {
            sub.to_string()
        } else {
            format!("{}/{}", self.prefix, sub)
        }
    }
}

/// Sidecar metadata describing the current replicated state. Mirrors the shape
/// of sqlink's `state.json` (generation + latest snapshot + sizes).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplicaState {
    /// Monotonic snapshot generation (incremented on each successful upload).
    pub generation: u64,
    /// Snapshot timestamp (unix milliseconds), also the object name stem.
    pub snapshot_ts: u64,
    /// Object key (relative to the prefix) of the newest snapshot.
    pub snapshot_key: String,
    /// Uncompressed `.duckdb` size in bytes.
    pub db_size: u64,
    /// Compressed (lz4) snapshot size in bytes.
    pub compressed_size: u64,
    /// Basename of the source database, used as the default restore target.
    pub db_name: String,
}

/// AWS-style configuration, resolved from the environment (the aws-credential
/// ext / sqlink s3-base convention): env keys first; region + endpoint derived.
struct S3Config {
    creds: Credentials,
    region: String,
    /// Endpoint origin, e.g. `https://s3.us-east-1.amazonaws.com` or a local
    /// `http://127.0.0.1:9000` for minio (`AWS_ENDPOINT_URL`).
    endpoint: String,
}

impl S3Config {
    fn from_env() -> Result<Self> {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| anyhow!("AWS_ACCESS_KEY_ID not set in the environment"))?;
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| anyhow!("AWS_SECRET_ACCESS_KEY not set in the environment"))?;
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.is_empty());
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        let endpoint = std::env::var("AWS_ENDPOINT_URL")
            .or_else(|_| std::env::var("AWS_ENDPOINT_URL_S3"))
            .unwrap_or_else(|_| format!("https://s3.{region}.amazonaws.com"))
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            creds: Credentials {
                access_key,
                secret_key,
                session_token,
            },
            region,
            endpoint,
        })
    }
}

/// A minimal blocking S3 client: path-style addressing, SigV4-signed PUT/GET.
struct S3Client {
    cfg: S3Config,
    bucket: String,
    http: reqwest::blocking::Client,
    /// Endpoint host (the `Host:` header + the signed `host` header).
    host_header: String,
    /// `true` when the endpoint is HTTPS (real S3); `false` for a local http
    /// endpoint override (minio). Real S3 is never downgraded off HTTPS.
    scheme: &'static str,
}

impl S3Client {
    fn new(cfg: S3Config, bucket: &str) -> Result<Self> {
        let (scheme, host_header) = if let Some(rest) = cfg.endpoint.strip_prefix("https://") {
            ("https", rest.to_string())
        } else if let Some(rest) = cfg.endpoint.strip_prefix("http://") {
            ("http", rest.to_string())
        } else {
            bail!("AWS_ENDPOINT_URL must start with http:// or https://: {:?}", cfg.endpoint);
        };
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            cfg,
            bucket: bucket.to_string(),
            http,
            host_header,
            scheme,
        })
    }

    /// Path-style object URL: `<endpoint>/<bucket>/<key>`.
    fn object_url(&self, key: &str) -> String {
        format!(
            "{}://{}/{}/{}",
            self.scheme,
            self.host_header,
            self.bucket,
            sigv4::uri_encode_path(key)
        )
    }

    /// The canonical URI used for signing (path-style: `/bucket/key`).
    fn canonical_uri(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, sigv4::uri_encode_path(key))
    }

    fn signed_headers(&self, method: &str, key: &str, payload_hash: &str) -> Vec<(String, String)> {
        let amz_date = sigv4::amz_date_now();
        let mut headers = vec![
            ("host".to_string(), self.host_header.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(tok) = &self.cfg.creds.session_token {
            headers.push(("x-amz-security-token".to_string(), tok.clone()));
        }
        let authz = sigv4::sign_v4(
            &self.cfg.creds,
            method,
            &self.cfg.region,
            "s3",
            &self.canonical_uri(key),
            "",
            &headers,
            payload_hash,
            &amz_date,
        );
        headers.push(("authorization".to_string(), authz));
        headers
    }

    fn put_object(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let payload_hash = hex::encode(Sha256::digest(&body));
        let headers = self.signed_headers("PUT", key, &payload_hash);
        let mut req = self.http.put(self.object_url(key));
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.body(body).send().with_context(|| format!("PUT {key}"))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().unwrap_or_default();
            bail!("PUT {key} failed: HTTP {status}: {txt}");
        }
        Ok(())
    }

    fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        let payload_hash = sigv4::EMPTY_PAYLOAD_SHA256;
        let headers = self.signed_headers("GET", key, payload_hash);
        let mut req = self.http.get(self.object_url(key));
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().with_context(|| format!("GET {key}"))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().unwrap_or_default();
            bail!("GET {key} failed: HTTP {status}: {txt}");
        }
        Ok(resp.bytes().with_context(|| format!("read body of {key}"))?.to_vec())
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Open a persistent single-file DuckDB through the wasm core, returning the
/// instantiated core + the live connection. Mirrors the httpd/ui open path
/// (extension autoinstall/autoload off; the cwd preopen is writable).
fn open_persistent(
    artifacts: &ComponentArtifacts,
    guest_db: &str,
    preopens: &[(&Path, &str)],
) -> Result<(CoreExecution, ResourceAny)> {
    let engine = build_engine()?;
    let wasi = build_wasi_ctx_inherit(&[String::from("ducklink-backup")], preopens)?;
    let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)
        .context("failed to instantiate the core component")?;
    let open_opts: Vec<(String, String)> = vec![
        ("autoinstall_known_extensions".to_string(), "false".to_string()),
        ("autoload_known_extensions".to_string(), "false".to_string()),
    ];
    let conn = core
        .with_database(|g, s| g.call_open_with_config(s, Some(guest_db), &open_opts))?
        .map_err(|e| anyhow!("open database {guest_db}: {e}"))?;
    Ok((core, conn))
}

fn checkpoint(core: &mut CoreExecution, conn: &ResourceAny) -> Result<()> {
    match core.with_database(|g, s| g.call_execute(s, conn.clone(), "CHECKPOINT")) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(anyhow!("CHECKPOINT failed: {}", duckerror_message(&e))),
        Err(e) => Err(anyhow!("CHECKPOINT trapped: {e}")),
    }
}

/// Take one snapshot: CHECKPOINT -> read the clean `.duckdb` -> lz4 -> upload
/// `snapshots/{ts}.duckdb.lz4`, then move the `latest` pointer + `state.json`.
/// `prev` is the last state (for the generation counter).
fn snapshot_once(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    host_db: &Path,
    s3: &S3Client,
    target: &S3Target,
    prev: &ReplicaState,
) -> Result<ReplicaState> {
    checkpoint(core, conn)?;

    let raw = std::fs::read(host_db)
        .with_context(|| format!("read database file {}", host_db.display()))?;
    let db_size = raw.len() as u64;
    // lz4 with the uncompressed length prepended, so restore needs no sidecar.
    let compressed = lz4_flex::compress_prepend_size(&raw);
    let compressed_size = compressed.len() as u64;

    let ts = now_millis();
    let snap_sub = format!("snapshots/{ts}.duckdb.lz4");
    s3.put_object(&target.key(&snap_sub), compressed)
        .with_context(|| format!("upload snapshot {snap_sub}"))?;

    let db_name = host_db
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("database.duckdb")
        .to_string();
    let state = ReplicaState {
        generation: prev.generation + 1,
        snapshot_ts: ts,
        snapshot_key: snap_sub.clone(),
        db_size,
        compressed_size,
        db_name,
    };

    // `latest` points at the snapshot key; `state.json` carries the metadata.
    s3.put_object(&target.key("latest"), snap_sub.clone().into_bytes())
        .context("update latest pointer")?;
    let state_json = serde_json::to_vec_pretty(&state).context("serialize state.json")?;
    s3.put_object(&target.key("state.json"), state_json).context("upload state.json")?;

    eprintln!(
        "ducklink backup: gen {} snapshot {} ({} -> {} bytes lz4) -> s3://{}/{}",
        state.generation,
        snap_sub,
        db_size,
        compressed_size,
        s3.bucket,
        target.key(&snap_sub),
    );
    Ok(state)
}

/// Run the backup. `interval` = `None` for a single one-shot snapshot, or
/// `Some(secs)` for continuous replication (snapshot every `secs`).
pub fn run_backup(
    artifacts: &ComponentArtifacts,
    host_db: &Path,
    guest_db: &str,
    target: &S3Target,
    interval: Option<u64>,
    preopens: &[(&Path, &str)],
) -> Result<()> {
    let s3 = S3Client::new(S3Config::from_env()?, &target.bucket)?;
    let (mut core, conn) = open_persistent(artifacts, guest_db, preopens)?;

    eprintln!(
        "ducklink backup: db={} -> s3://{}/{} (endpoint {}, region {})",
        host_db.display(),
        target.bucket,
        target.prefix,
        s3.cfg.endpoint,
        s3.cfg.region,
    );

    let mut state = ReplicaState::default();
    state = snapshot_once(&mut core, &conn, host_db, &s3, target, &state)?;

    let Some(secs) = interval else {
        return Ok(());
    };
    eprintln!("ducklink backup: continuous mode, snapshot every {secs}s (ctrl-c to stop)");
    loop {
        std::thread::sleep(Duration::from_secs(secs));
        match snapshot_once(&mut core, &conn, host_db, &s3, target, &state) {
            Ok(next) => state = next,
            Err(e) => eprintln!("ducklink backup: snapshot error (will retry next interval): {e:#}"),
        }
    }
}

/// Restore: read `latest` (or `state.json` for the db name default), pull the
/// snapshot, lz4-decompress, and write it to `dest`. Does not open the DB —
/// the caller opens the restored file normally.
pub fn run_restore(target: &S3Target, dest: Option<&Path>) -> Result<PathBuf> {
    let s3 = S3Client::new(S3Config::from_env()?, &target.bucket)?;

    // The state.json is best-effort (db name default + reporting); `latest` is
    // authoritative for which snapshot to pull.
    let state: Option<ReplicaState> = s3
        .get_object(&target.key("state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());

    let snap_key = String::from_utf8(
        s3.get_object(&target.key("latest"))
            .context("read latest pointer (no snapshots yet?)")?,
    )
    .context("latest pointer is not valid utf-8")?;
    let snap_key = snap_key.trim().to_string();

    let dest: PathBuf = match dest {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(
            state
                .as_ref()
                .map(|s| s.db_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "restored.duckdb".to_string()),
        ),
    };

    let compressed = s3
        .get_object(&target.key(&snap_key))
        .with_context(|| format!("download snapshot {snap_key}"))?;
    let raw = lz4_flex::decompress_size_prepended(&compressed)
        .map_err(|e| anyhow!("lz4 decompress {snap_key}: {e}"))?;

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    std::fs::write(&dest, &raw)
        .with_context(|| format!("write restored database {}", dest.display()))?;

    eprintln!(
        "ducklink restore: snapshot {} ({} bytes) -> {}{}",
        snap_key,
        raw.len(),
        dest.display(),
        state
            .as_ref()
            .map(|s| format!(" (generation {})", s.generation))
            .unwrap_or_default(),
    );
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_s3_targets() {
        let t = S3Target::parse("s3://my-bucket/some/prefix").unwrap();
        assert_eq!(t.bucket, "my-bucket");
        assert_eq!(t.prefix, "some/prefix");
        assert_eq!(t.key("latest"), "some/prefix/latest");

        let t2 = S3Target::parse("s3://only-bucket").unwrap();
        assert_eq!(t2.bucket, "only-bucket");
        assert_eq!(t2.prefix, "");
        assert_eq!(t2.key("state.json"), "state.json");

        let t3 = S3Target::parse("s3://b/p/").unwrap();
        assert_eq!(t3.prefix, "p");

        assert!(S3Target::parse("https://x/y").is_err());
        assert!(S3Target::parse("s3:///nobucket").is_err());
    }

    #[test]
    fn lz4_roundtrip() {
        let data = b"the quick brown duck jumps over the lazy lake".repeat(100);
        let c = lz4_flex::compress_prepend_size(&data);
        let d = lz4_flex::decompress_size_prepended(&c).unwrap();
        assert_eq!(d, data);
    }
}
