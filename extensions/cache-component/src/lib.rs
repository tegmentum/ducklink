//! `ducklink:cache` wasm engine — the wasm-side counterpart to the
//! native `duckdb-cache` extension (see
//! https://github.com/tegmentum/duckdb-cache).
//!
//! Thin, hand-rolled shim (same pattern as `fieldbook-component`: the
//! standard `duckdb_shim!` macro can't hook a load-time bootstrap, and
//! this extension has three non-standard load-time steps beyond scalar
//! registration — install the resolver fn pointer into `cache-core`,
//! run the metadata-catalog CREATE TABLE, and register the two `cache`
//! overloads under the single name `"cache"`).
//!
//! # Backends
//!
//! * `file://` — pass-through (handled inside `cache-core`).
//! * `http://` / `https://` — full ETag / If-None-Match / Last-Modified
//!   / If-Modified-Since / 304 dance over `std::net::TcpStream`
//!   (matches `httpclient-component`'s TLS stack: rustls + rustls-
//!   rustcrypto + webpki-roots).
//! * `s3://` / `az://` / `azure://` / `gs://` — wac-composed cloud
//!   backends. Each rides a sibling `component:*-wasm` component
//!   plugged in at build time (see the `cache` recipe in
//!   ../../../Makefile). Auth is per-backend (SigV4 for s3, SharedKey
//!   / SAS / anonymous for azure, service-account JWT / access-token /
//!   anonymous for gcs); all three run head-then-get with ETag
//!   revalidation against the shared catalog + content-addressed
//!   store.
//!
//! # Persistent metadata
//!
//! The metadata catalog rides `sqlite:extension/spi` (imported from
//! sqlite-wasm's WIT). Bytes-on-disk layout stays byte-identical to
//! the native — same schema in `cache-core::CREATE_CACHE_ENTRIES`,
//! same content-addressed `objects/<hh>/<rest>` shard, same
//! `<sha256-of-uri>.lock` path (see the locking caveat below).
//!
//! # Locking (v0.2)
//!
//! The native extension takes an `fs2::FileExt::lock_exclusive` on
//! `<cache_root>/locks/<sha256-of-uri>.lock` to serialise concurrent
//! misses on the same URI. WASI 0.2 has no flock; the wasm arm
//! therefore imports the host-provided
//! `duckdb:extension/file-lock@5.0.0` interface (backed by fs2 on the
//! host), acquiring the SAME lock file path the native uses. Two
//! processes racing on the same URI now behave identically to the
//! native: the loser blocks in `acquire-exclusive`, then observes the
//! winner's published entry on a re-lookup and returns without ever
//! going to the network. The lock is ADVISORY: if a host somehow does
//! not wire the file-lock import, the resolver still produces correct
//! bytes (content-addressed store + rename-atomic publish), just with
//! the pre-v0.2 duplicate-download cost.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String as WitString;
use wit_bindgen::rt::vec::Vec as WitVec;

use cache_core::{Config, Policy};
use datalink_extcore::{ExtCore as _, NeutralType, NeutralValue, NullHandling};

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension-cache",
    generate_all,
});

use duckdb::extension::{file_lock, runtime, types};
use exports::duckdb::extension::guest;
use sqlite::extension::{spi as sqlite_spi, types as sqlite_types};
// s3-wasm bindings: satisfied at compose time by `wac plug`ing the
// `s3-wasm` component (pre-composed with `aws-sigv4-wasm`) into
// `cache.wasm`. See the `cache` recipe in ../../../Makefile.
use component::s3_wasm::{s3_base, s3_types};
// azure-wasm bindings: satisfied at compose time by `wac plug`ing the
// `azure-wasm` component into `cache.wasm`. See the `cache` recipe in
// ../../../Makefile. Azure has no separate signer sidecar — signing
// happens inside azure-wasm from the `credentials` record.
use component::azure_wasm::{blob_base, blob_types};
// gcs-wasm bindings: satisfied at compose time by `wac plug`ing the
// `gcs-wasm` component into `cache.wasm`. See the `cache` recipe in
// ../../../Makefile. GCS has no separate signer sidecar either — the
// RS256 JWT signing happens inside gcs-wasm via RustCrypto `rsa`.
// Renamed on import to avoid a name clash with the azure-wasm modules.
use component::gcs_wasm::{
    blob_anon as gcs_blob_anon, blob_base as gcs_blob_base, blob_oauth as gcs_blob_oauth,
    blob_types as gcs_blob_types,
};
// wasi:http surface for the HTTP/HTTPS fetch backend. Both TLS and
// plaintext transport now go through the host's wasmtime-wasi-http
// wiring (see `ducklink-host::add_only_http_to_linker_sync`, commit
// aaa5891) — the same interface s3-wasm consumes for its HTTPS
// requests. One transport, one host wiring, no in-wasm rustls.
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

// ---------------------------------------------------------------------------
// Handle table (u32 -> DECLS index). Same layout the `duckdb_shim!`
// macro uses; the fieldbook shim carries the same skeleton.
// ---------------------------------------------------------------------------

fn handles() -> &'static Mutex<HashMap<u32, usize>> {
    static T: OnceLock<Mutex<HashMap<u32, usize>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

/// Monotonic tmp-name suffix. `std::process::id()` panics under wasip1 (the
/// stdlib's WASI shim aborts on unsupported syscalls); a static counter is
/// unique-within-process which is all the staging path needs — the sha256
/// prefix + this suffix collide only if two threads happen to publish the
/// same content-hash simultaneously, and the two-step rename tolerates the
/// AlreadyExists case anyway.
static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Marshalling: Duckvalue <-> NeutralValue.
// FROZEN set + `complex(...)` escape hatch, kept in sync with
// datalink-extcore/src/shim_duckdb.rs::to_neutral/from_neutral.
// ---------------------------------------------------------------------------

fn to_neutral(v: &types::Duckvalue) -> NeutralValue {
    match v {
        types::Duckvalue::Null => NeutralValue::Null,
        types::Duckvalue::Boolean(b) => NeutralValue::Boolean(*b),
        types::Duckvalue::Int64(n) => NeutralValue::Int64(*n),
        types::Duckvalue::Float64(f) => NeutralValue::Float64(*f),
        types::Duckvalue::Text(s) => NeutralValue::Text(String::from(s.as_str())),
        types::Duckvalue::Blob(b) => NeutralValue::Blob(b.clone()),
        types::Duckvalue::Complex(c) => NeutralValue::Complex {
            type_expr: String::from(c.type_expr.as_str()),
            json: String::from(c.json.as_str()),
        },
        other => NeutralValue::Complex {
            type_expr: String::from("UNSUPPORTED"),
            json: format!("{:?}", other),
        },
    }
}

fn from_neutral(v: NeutralValue) -> types::Duckvalue {
    match v {
        NeutralValue::Null => types::Duckvalue::Null,
        NeutralValue::Boolean(b) => types::Duckvalue::Boolean(b),
        NeutralValue::Int64(n) => types::Duckvalue::Int64(n),
        NeutralValue::Float64(f) => types::Duckvalue::Float64(f),
        NeutralValue::Text(s) => types::Duckvalue::Text(s.into()),
        NeutralValue::Blob(b) => types::Duckvalue::Blob(b),
        NeutralValue::Complex { type_expr, json } => {
            types::Duckvalue::Complex(types::Complexvalue {
                type_expr: type_expr.into(),
                json: json.into(),
            })
        }
    }
}

fn ntype_to_logical(t: &NeutralType) -> types::Logicaltype {
    match t {
        NeutralType::Boolean => types::Logicaltype::Boolean,
        NeutralType::Int64 => types::Logicaltype::Int64,
        NeutralType::Float64 => types::Logicaltype::Float64,
        NeutralType::Text => types::Logicaltype::Text,
        NeutralType::Blob => types::Logicaltype::Blob,
        NeutralType::Complex(e) => types::Logicaltype::Complex(e.clone().into()),
    }
}

fn duckerr(e: String) -> types::Duckerror {
    types::Duckerror::Invalidargument(e)
}

// ---------------------------------------------------------------------------
// sqlite:extension/spi bridge helpers.
//
// The SPI surface takes bound parameters; the resolver code below binds
// every value explicitly (no interpolation into SQL text), matching the
// native's rusqlite-parameterised statements.
// ---------------------------------------------------------------------------

fn spi_err(e: sqlite_types::SqliteError) -> String {
    format!(
        "cache metadata: sqlite [{}/{}] {}",
        e.code, e.extended_code, e.message
    )
}

fn spi_text(s: &str) -> sqlite_types::SqlValue {
    sqlite_types::SqlValue::Text(s.into())
}

fn spi_int(n: i64) -> sqlite_types::SqlValue {
    sqlite_types::SqlValue::Integer(n)
}

fn spi_null() -> sqlite_types::SqlValue {
    sqlite_types::SqlValue::Null
}

fn spi_str(v: &sqlite_types::SqlValue) -> Option<String> {
    match v {
        sqlite_types::SqlValue::Text(s) => Some(std::string::String::from(s.as_str())),
        sqlite_types::SqlValue::Null => None,
        // The catalog schema pins these columns to TEXT / INTEGER; if
        // the shape drifts, produce a placeholder rather than panicking
        // inside a DuckDB scalar callback.
        other => Some(format!("{:?}", other)),
    }
}

#[allow(dead_code)]
fn spi_i64(v: &sqlite_types::SqlValue) -> Option<i64> {
    match v {
        sqlite_types::SqlValue::Integer(n) => Some(*n),
        sqlite_types::SqlValue::Null => None,
        _ => None,
    }
}

fn spi_bootstrap() -> Result<(), String> {
    // Point the SPI's shared connection at the on-disk metadata.db BEFORE
    // running any DDL. sqlite-lib's default connection is `:memory:` and
    // per-wasm-instance; without this every process (or every re-load)
    // would have its own catalog, defeating cross-process cache sharing
    // and making the file-lock re-lookup-under-lock always miss.
    let root = cache_root()?;
    ensure_dirs(&root)?;
    let db_path = root.join("metadata.db");

    // Cross-process bootstrap lock. wasivfs (the SQLite VFS baked into
    // sqlite-lib.wasm) does NOT implement real xLock/xUnlock — those are
    // in-memory bookkeeping only, per its "single-process use" contract.
    // Without an external lock, N concurrent ducklink processes each
    // run `CREATE TABLE IF NOT EXISTS` against the same file at load-
    // time, and the interleaved writes corrupt the pager (SQLITE_CORRUPT
    // "database disk image is malformed"). Serialise on a file lock
    // just for the bootstrap window; steady-state reads/writes below
    // still ride on the per-URI lock the resolver takes around the
    // fetch, and this lock is dropped before any DuckDB scalar runs.
    let bootstrap_lock_path = root.join("locks").join("metadata-bootstrap.lock");
    let _bootstrap_lock: Option<file_lock::LockHandle> =
        file_lock::acquire_exclusive(&bootstrap_lock_path.to_string_lossy()).ok();

    sqlite_spi::open_db(
        db_path
            .to_str()
            .ok_or_else(|| format!("cache metadata: non-utf8 db path {}", db_path.display()))?,
    )
    .map_err(spi_err)?;
    // Two DDLs; batch them so the SPI runs them under one prepare/step
    // pair (or falls back to two prepares if the host does not support
    // `execute-batch` compounds — either way idempotent).
    let sql = cache_core::schema_sql();
    sqlite_spi::execute_batch(&sql).map(|_| ()).map_err(spi_err)
}

// ---------------------------------------------------------------------------
// Filesystem helpers.
//
// std::fs on wasm32-wasip1 is transparently backed by WASI preopens the
// host passes at instantiation; the ducklink CLI is expected to preopen
// the cache root (or the current working directory containing it). Any
// I/O error here surfaces to the caller as a scalar-side error string,
// matching the native's `Result<_, String>` shape.
// ---------------------------------------------------------------------------

fn cache_root() -> Result<std::path::PathBuf, String> {
    // Native honours `DUCKLINK_LOCAL_CACHE` / `DUCKLINK_GLOBAL_CACHE` for
    // the Local / Global scopes respectively, defaulting to
    // `<cwd>/.ducklink/cache/` for Local and the platform cache dir for
    // Global (via `directories::ProjectDirs`). WASI has no reliable
    // platform-cache concept, so the wasm arm always resolves to
    // `<DUCKLINK_LOCAL_CACHE or cwd>/.ducklink/cache`, treating Global as
    // an alias of Local when the env var isn't set.
    if let Ok(v) = std::env::var("DUCKLINK_LOCAL_CACHE") {
        return Ok(std::path::PathBuf::from(v));
    }
    if let Ok(v) = std::env::var("DUCKLINK_GLOBAL_CACHE") {
        return Ok(std::path::PathBuf::from(v));
    }
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cache: cannot read cwd (no WASI preopen?): {e}"))?;
    Ok(cwd.join(".ducklink").join("cache"))
}

fn ensure_dirs(root: &std::path::Path) -> Result<(), String> {
    for sub in ["objects", "locks", "tmp"] {
        let d = root.join(sub);
        std::fs::create_dir_all(&d).map_err(|e| format!("cache: creating {}: {e}", d.display()))?;
    }
    Ok(())
}

fn blob_path(root: &std::path::Path, hash: &str) -> std::path::PathBuf {
    root.join("objects").join(&hash[..2]).join(&hash[2..])
}

fn path_to_file_uri(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    format!("file://{s}")
}

// ---------------------------------------------------------------------------
// HTTP fetch.
//
// Runs over `wasi:http/outgoing-handler` — the same host-provided
// transport `component:s3-wasm` uses for HTTPS. TLS is handled by the
// host (wasmtime-wasi-http) so the wasm arm carries no rustls / no
// embedded CA bundle. Full cache-control semantics are preserved:
// If-None-Match / If-Modified-Since on request, ETag / Last-Modified /
// Cache-Control max-age / Expires harvested from response, and a 304
// returned as a body-less `HttpResp` with `status = 304`.
// ---------------------------------------------------------------------------

struct HttpResp {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    max_age: Option<u64>,
    expires: Option<String>,
}

/// Parse `http(s)://authority[/path[?query]]` into
/// `(scheme, authority, path_with_query)`. Authority is the raw
/// `host[:port]` string (wasi:http accepts it unparsed).
fn parse_url(url: &str) -> Option<(Scheme, String, String)> {
    let url = url.trim();
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        (Scheme::Https, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (Scheme::Http, r)
    } else {
        return None;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let path_with_query = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    Some((scheme, authority.to_string(), path_with_query))
}

/// Case-insensitive header lookup on the `(name, value)` list wasi:http
/// hands back from `Fields::entries()`.
fn header_of(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn parse_max_age(cache_control: Option<&str>) -> Option<u64> {
    let cc = cache_control?;
    for d in cc.split(',') {
        let d = d.trim();
        if let Some(rest) = d.strip_prefix("max-age=") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// GET `url` over `wasi:http/outgoing-handler`. Conditional-request
/// headers (If-None-Match / If-Modified-Since) go on the outgoing
/// request when supplied; response headers (etag / last-modified /
/// cache-control max-age / expires) are harvested to drive cache-core's
/// revalidation policy.
fn fetch_http_via_wasi(
    url: &str,
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
) -> Result<HttpResp, String> {
    let (scheme, authority, path_with_query) =
        parse_url(url).ok_or_else(|| format!("cache http backend: not an http(s) URL: {url}"))?;

    // Build the request headers. `Fields::from_list` takes
    // `(name, Vec<u8>)` pairs.
    let ua = format!("ducklink-cache/{}", env!("CARGO_PKG_VERSION"));
    let mut header_entries: Vec<(String, Vec<u8>)> = vec![
        ("user-agent".to_string(), ua.into_bytes()),
        ("accept".to_string(), b"*/*".to_vec()),
    ];
    if let Some(v) = if_none_match {
        header_entries.push(("if-none-match".to_string(), v.as_bytes().to_vec()));
    }
    if let Some(v) = if_modified_since {
        header_entries.push(("if-modified-since".to_string(), v.as_bytes().to_vec()));
    }
    let fields = Fields::from_list(&header_entries)
        .map_err(|e| format!("cache http backend: build headers for {url} failed: {e:?}"))?;

    let request = OutgoingRequest::new(fields);
    request
        .set_method(&Method::Get)
        .map_err(|_| format!("cache http backend: set method for {url} failed"))?;
    request
        .set_scheme(Some(&scheme))
        .map_err(|_| format!("cache http backend: set scheme for {url} failed"))?;
    request
        .set_authority(Some(&authority))
        .map_err(|_| format!("cache http backend: set authority for {url} failed"))?;
    request
        .set_path_with_query(Some(&path_with_query))
        .map_err(|_| format!("cache http backend: set path for {url} failed"))?;

    // Dispatch. `handle` returns a `future-incoming-response`; poll it
    // until the host resolves it, mirroring the shape s3-wasm uses.
    let future_response = outgoing_handler::handle(request, None)
        .map_err(|e| format!("cache http backend: request to {url} failed: {e:?}"))?;

    let response = loop {
        if let Some(result) = future_response.get() {
            break result
                .map_err(|_| {
                    format!("cache http backend: response future for {url} already consumed")
                })?
                .map_err(|e| format!("cache http backend: request to {url} failed: {e:?}"))?;
        }
        // Block until the host has more work for us.
        future_response.subscribe().block();
    };

    let status = response.status();
    let header_list: Vec<(String, String)> = response
        .headers()
        .entries()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
        .collect();

    let etag = header_of(&header_list, "etag");
    let last_modified = header_of(&header_list, "last-modified");
    let cc = header_of(&header_list, "cache-control");
    let max_age = parse_max_age(cc.as_deref());
    let expires = header_of(&header_list, "expires");

    // 304 has no body; skip the body read.
    if status == 304 {
        return Ok(HttpResp {
            status,
            body: Vec::new(),
            etag,
            last_modified,
            max_age,
            expires,
        });
    }

    // Read the body via the incoming-body's input-stream.
    // `incoming-body::stream()` must be dropped before we call
    // `IncomingBody::finish`; we skip the trailer collection here since
    // cache-core has no use for HTTP trailers.
    let incoming_body = response
        .consume()
        .map_err(|_| format!("cache http backend: consume body for {url} failed"))?;
    let stream = incoming_body
        .stream()
        .map_err(|_| format!("cache http backend: open body stream for {url} failed"))?;
    let mut body = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    // A zero-length read on a still-open stream is a
                    // "no bytes right now" signal — block on the
                    // stream's readiness pollable and try again.
                    stream.subscribe().block();
                    continue;
                }
                body.extend_from_slice(&chunk);
            }
            Err(StreamError::Closed) => break,
            Err(e) => {
                return Err(format!(
                    "cache http backend: read body for {url} failed: {e:?}"
                ));
            }
        }
    }

    Ok(HttpResp {
        status,
        body,
        etag,
        last_modified,
        max_age,
        expires,
    })
}

// ---------------------------------------------------------------------------
// The resolver itself.
//
// Ports duckdb-cache/src/resolver.rs to the wasm shim's I/O primitives:
// sqlite-spi for the catalog, std::fs for staging + publish, the HTTP
// helpers above for fetch. Kept in one place (not split into per-backend
// modules) because the v0 backend set is 2 items and duplicating the
// module structure of the native for two implementations doesn't earn
// the file split.
// ---------------------------------------------------------------------------

const NOT_YET_SUPPORTED_HINT: &str = "supported schemes in v0: file://, http://, https://, s3://, \
     az://, azure://, gs://.";

struct CachedEntry {
    etag: Option<String>,
    last_modified: Option<String>,
    content_hash: String,
    resolved_path: String,
}

fn lookup(cache_name: &str, source_uri: &str) -> Result<Option<CachedEntry>, String> {
    let params: WitVec<sqlite_types::SqlValue> =
        vec![spi_text(cache_name), spi_text(source_uri)].into();
    let r = sqlite_spi::execute(
        "SELECT etag, last_modified, content_hash, resolved_path \
         FROM cache_entries WHERE cache_name = ?1 AND source_uri = ?2",
        &params,
    )
    .map_err(spi_err)?;
    let row = match r.rows.first() {
        Some(row) => row,
        None => return Ok(None),
    };
    let etag = row.first().and_then(spi_str);
    let last_modified = row.get(1).and_then(spi_str);
    let content_hash = row
        .get(2)
        .and_then(spi_str)
        .ok_or_else(|| String::from("cache metadata: content_hash missing"))?;
    let resolved_path = row
        .get(3)
        .and_then(spi_str)
        .ok_or_else(|| String::from("cache metadata: resolved_path missing"))?;
    Ok(Some(CachedEntry {
        etag,
        last_modified,
        content_hash,
        resolved_path,
    }))
}

#[allow(clippy::too_many_arguments)]
fn upsert(
    cache_name: &str,
    source_uri: &str,
    scheme: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    content_hash: &str,
    content_length: Option<i64>,
    resolved_path: &str,
    now: &str,
    expires_at: Option<&str>,
) -> Result<(), String> {
    let params: WitVec<sqlite_types::SqlValue> = vec![
        spi_text(cache_name),
        spi_text(source_uri),
        spi_text(scheme),
        etag.map(spi_text).unwrap_or_else(spi_null),
        last_modified.map(spi_text).unwrap_or_else(spi_null),
        spi_text(content_hash),
        content_length.map(spi_int).unwrap_or_else(spi_null),
        spi_text(resolved_path),
        spi_text(now),
        spi_text(now),
        expires_at.map(spi_text).unwrap_or_else(spi_null),
    ]
    .into();
    sqlite_spi::execute(
        "INSERT INTO cache_entries \
            (cache_name, source_uri, scheme, etag, last_modified, \
             content_hash, content_length, resolved_path, \
             retrieved_at, validated_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT(cache_name, source_uri) DO UPDATE SET \
            scheme=excluded.scheme, \
            etag=excluded.etag, \
            last_modified=excluded.last_modified, \
            content_hash=excluded.content_hash, \
            content_length=excluded.content_length, \
            resolved_path=excluded.resolved_path, \
            retrieved_at=excluded.retrieved_at, \
            validated_at=excluded.validated_at, \
            expires_at=excluded.expires_at",
        &params,
    )
    .map(|_| ())
    .map_err(spi_err)
}

fn compute_now() -> String {
    // ISO-8601 in UTC. WASI clocks are Y2K-safe; format the epoch by
    // hand so we don't drag a full time crate into the extension.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_epoch_utc(secs)
}

fn compute_expires_from_max_age(now: &str, max_age: Option<u64>) -> Option<String> {
    let max_age = max_age?;
    let secs = parse_epoch_utc(now)?;
    Some(format_epoch_utc(secs + max_age as i64))
}

fn parse_epoch_utc(s: &str) -> Option<i64> {
    // Reads back what `format_epoch_utc` writes. Only used to add a
    // max-age offset for the expires_at derivation.
    let (date, rest) = s.split_once('T')?;
    let (time, _) = rest.split_once('Z')?;
    let mut ds = date.split('-');
    let y: i64 = ds.next()?.parse().ok()?;
    let m: i64 = ds.next()?.parse().ok()?;
    let d: i64 = ds.next()?.parse().ok()?;
    let mut ts = time.split(':');
    let hh: i64 = ts.next()?.parse().ok()?;
    let mm: i64 = ts.next()?.parse().ok()?;
    let ss: i64 = ts.next()?.parse().ok()?;
    Some(days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss)
}

fn format_epoch_utc(secs: i64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_days(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

/// Howard Hinnant's `days_from_civil` — proleptic Gregorian, epoch
/// 1970-01-01. Range covers ±year millions; we only need year 2020+
/// but the exact formula is cheap enough to keep verbatim.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Inverse of `days_from_civil` — given a Unix epoch second, returns
/// `(year, month, day, hour, minute, second)` in UTC.
fn civil_from_days(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hh, mm, ss)
}

/// Full v0 resolve loop. Ported from `duckdb-cache/src/resolver.rs`
/// with `sqlite:extension/spi` in place of rusqlite and `std::fs` in
/// place of `fs2::FileExt` (no flock — see the module-level caveat).
fn resolver(cfg: &Config, uri: &str) -> Result<String, String> {
    // `file://` was already handled inside cache-core; anything reaching
    // here needs a fetch backend.
    let scheme = uri
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();

    // Cloud stubs (parity with duckdb-cache/src/backends/mod.rs::dispatch).
    // Note: s3:// and az:///azure:// are intercepted in `cache_scalar`
    // before we reach here, because backend-specific config keys don't
    // round-trip through cache-core's strict `Config::from_json`
    // (deny_unknown_fields). If a cloud URI does reach this point (only
    // possible for the arity-1 overload `cache(uri)` where no config is
    // supplied), fall through to the backend dispatch with defaults.
    if scheme == "s3" {
        return resolve_s3(cfg, S3Params::default(), uri, &scheme);
    }
    if scheme == "az" || scheme == "azure" {
        return resolve_azure(cfg, AzureParams::default(), uri, &scheme);
    }
    if scheme == "gs" {
        return resolve_gcs(cfg, GcsParams::default(), uri, &scheme);
    }
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "cache: scheme {scheme:?} not supported. {NOT_YET_SUPPORTED_HINT}"
        ));
    }

    let root = cache_root()?;
    ensure_dirs(&root)?;

    let existing = lookup(&cfg.name, uri)?;

    // Offline: never touch the network.
    if cfg.policy == Policy::Offline {
        return match existing {
            Some(e) => {
                enforce_sha_pin(cfg, &e.content_hash, uri)?;
                Ok(path_to_file_uri(std::path::Path::new(&e.resolved_path)))
            }
            None => Err(format!(
                "cache: policy \"offline\" and no cached entry for {uri}"
            )),
        };
    }

    // Immutable: pinned sha256 already on disk means done.
    if cfg.policy == Policy::Immutable {
        if let Some(expected) = &cfg.sha256 {
            let pinned = blob_path(&root, expected);
            if pinned.is_file() {
                if existing.is_none() {
                    // Blob published by a prior run under a different
                    // (cache_name, uri); re-record.
                    let now = compute_now();
                    let len = std::fs::metadata(&pinned).ok().map(|m| m.len() as i64);
                    upsert(
                        &cfg.name,
                        uri,
                        &scheme,
                        None,
                        None,
                        expected,
                        len,
                        &pinned.to_string_lossy(),
                        &now,
                        None,
                    )?;
                }
                return Ok(path_to_file_uri(&pinned));
            }
        }
    }

    // Auto: honour the caller TTL against `validated_at` — v0 skips the
    // full expires-at / TTL machinery and always revalidates on Auto (a
    // conservative choice that never returns stale bytes; the native
    // arm's TTL fast path is a follow-up).
    // Everything below crosses the network.

    // v0.2 lock: serialise concurrent misses on the SAME source URI so
    // only one process pays the download cost. Peers block in
    // `acquire-exclusive`, then observe the winner's published entry via
    // the re-lookup below and short-circuit without touching the network.
    // The lock is ADVISORY -- if the host does not wire file-lock, we
    // just fall through (correct, but with duplicate-download cost, i.e.
    // pre-v0.2 behaviour).
    let lock_path = root
        .join("locks")
        .join(format!("{}.lock", cache_core::sha256_hex(uri.as_bytes())));
    let _uri_lock: Option<file_lock::LockHandle> =
        match file_lock::acquire_exclusive(&lock_path.to_string_lossy()) {
            Ok(handle) => {
                // Re-lookup under lock: a peer may have finished the
                // download while we blocked. If so, return its entry.
                if let Some(now_cached) = lookup(&cfg.name, uri)? {
                    enforce_sha_pin(cfg, &now_cached.content_hash, uri)?;
                    return Ok(path_to_file_uri(std::path::Path::new(
                        &now_cached.resolved_path,
                    )));
                }
                Some(handle)
            }
            Err(_) => {
                // Advisory: proceed unlocked. The publish step is
                // rename-atomic and the catalog upsert is idempotent, so
                // correctness is preserved; we just may duplicate the
                // fetch cost with a racing peer.
                None
            }
        };

    let (if_none_match, if_modified_since) = match existing.as_ref() {
        Some(e) => (e.etag.clone(), e.last_modified.clone()),
        None => (None, None),
    };
    let resp = fetch_http_via_wasi(uri, if_none_match.as_deref(), if_modified_since.as_deref())?;
    let now = compute_now();

    if resp.status == 304 {
        let e = existing.ok_or_else(|| {
            format!("cache: {uri} returned 304 but no cached entry exists (unexpected)")
        })?;
        let expires_at = compute_expires_from_max_age(&now, resp.max_age).or(resp.expires.clone());
        upsert(
            &cfg.name,
            uri,
            &scheme,
            resp.etag.as_deref().or(e.etag.as_deref()),
            resp.last_modified.as_deref().or(e.last_modified.as_deref()),
            &e.content_hash,
            None,
            &e.resolved_path,
            &now,
            expires_at.as_deref(),
        )?;
        enforce_sha_pin(cfg, &e.content_hash, uri)?;
        return Ok(path_to_file_uri(std::path::Path::new(&e.resolved_path)));
    }

    if !(200..300).contains(&resp.status) {
        return Err(format!("cache http backend: {} for {uri}", resp.status));
    }

    // Publish the body. Compute the hash, rename tmp -> objects/<hh>/<rest>.
    let hash = cache_core::sha256_hex(&resp.body);
    if let Some(expected) = &cfg.sha256 {
        if &hash != expected {
            return Err(format!(
                "cache: sha256 mismatch for {uri}: expected {expected}, got {hash}"
            ));
        }
    }
    let content_length = resp.body.len() as i64;
    let final_path = blob_path(&root, &hash);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cache staging: creating {}: {e}", parent.display()))?;
    }
    if !final_path.exists() {
        // Two-step publish: write to tmp/, then rename. `create_dir_all`
        // on tmp/ was done by `ensure_dirs`.
        let tmp_name = format!("{}.{}", hash, TMP_COUNTER.fetch_add(1, Ordering::Relaxed));
        let tmp_path = root.join("tmp").join(&tmp_name);
        std::fs::write(&tmp_path, &resp.body)
            .map_err(|e| format!("cache staging: writing tmp: {e}"))?;
        match std::fs::rename(&tmp_path, &final_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp_path);
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!(
                    "cache staging: renaming to {}: {err}",
                    final_path.display()
                ));
            }
        }
    }

    let expires_at = compute_expires_from_max_age(&now, resp.max_age).or(resp.expires.clone());
    upsert(
        &cfg.name,
        uri,
        &scheme,
        resp.etag.as_deref(),
        resp.last_modified.as_deref(),
        &hash,
        Some(content_length),
        &final_path.to_string_lossy(),
        &now,
        expires_at.as_deref(),
    )?;
    Ok(path_to_file_uri(&final_path))
}

// ---------------------------------------------------------------------------
// s3 backend (composed via `component:s3-wasm/{s3-base,s3-aws}`).
//
// The s3-wasm surface exports two orthogonal call families:
//
//   * `s3-base` — the S3 REST core; works against any S3-compatible
//     endpoint (real AWS, MinIO, Cloudflare R2, DigitalOcean Spaces).
//     Composed at build time so signed URLs go over the imported
//     `wasi:http/outgoing-handler` transport rather than the raw
//     TcpStream + rustls the http/https backend uses.
//   * `s3-aws`  — AWS-only extensions (presign, tagging, restore,
//     select). The cache resolver only calls `s3-base` for read
//     operations; `s3-aws` is imported to keep the composition
//     complete (so if the resolver grows AWS-specific paths later,
//     the component is already wired).
//
// Config JSON extends the base `Config` with four s3-specific keys
// that mirror the native duckdb-cache/backends/s3.rs surface:
//
//   * `endpoint`   — override the S3 endpoint URL (MinIO, R2, custom).
//                    Default: infer from `region` -> `s3.<region>.amazonaws.com`.
//   * `region`     — signing / URL region. Default: `AWS_REGION` env
//                    var or `us-east-1`.
//   * `version_id` — pin a specific object version on a versioned bucket.
//                    Sent via `versionId` query parameter (not yet
//                    threaded through s3-wasm's get-object; kept in
//                    the struct so the parse is complete and a
//                    follow-up can wire it into a range/opts add-on).
//   * `anonymous`  — skip credential resolution entirely. Public
//                    buckets (NOAA / Landsat / etc.) don't need
//                    SigV4 headers.
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
struct S3Params {
    endpoint: Option<String>,
    region: Option<String>,
    version_id: Option<String>,
    anonymous: bool,
    path_style: bool,
}

impl S3Params {
    /// Parse the s3-specific fields off a JSON blob (leniently — the
    /// full JSON is also handed to cache-core's strict parser for the
    /// name/policy/sha256/ttl fields). Empty / missing config -> defaults.
    fn from_json(s: &str) -> Result<Self, String> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("cache s3 config: {e}"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| String::from("cache s3 config: expected a JSON object"))?;
        let mut out = Self::default();
        if let Some(ep) = obj.get("endpoint") {
            out.endpoint = Some(
                ep.as_str()
                    .ok_or_else(|| String::from("cache s3 config: endpoint must be a string"))?
                    .to_string(),
            );
        }
        if let Some(rg) = obj.get("region") {
            out.region = Some(
                rg.as_str()
                    .ok_or_else(|| String::from("cache s3 config: region must be a string"))?
                    .to_string(),
            );
        }
        if let Some(v) = obj.get("version_id") {
            out.version_id = Some(
                v.as_str()
                    .ok_or_else(|| String::from("cache s3 config: version_id must be a string"))?
                    .to_string(),
            );
        }
        if let Some(v) = obj.get("anonymous") {
            out.anonymous = v
                .as_bool()
                .ok_or_else(|| String::from("cache s3 config: anonymous must be a boolean"))?;
        }
        if let Some(v) = obj.get("path_style") {
            out.path_style = v
                .as_bool()
                .ok_or_else(|| String::from("cache s3 config: path_style must be a boolean"))?;
        }
        Ok(out)
    }
}

/// Strip the s3-specific keys from a config JSON blob so cache-core's
/// strict parser can still handle the shared knobs (name, policy,
/// sha256, ttl). Returns an empty string when nothing remains, which
/// cache-core treats as `Config::default()`.
fn strip_s3_keys(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        return Ok(String::new());
    }
    let mut v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("cache s3 config: {e}"))?;
    if let Some(obj) = v.as_object_mut() {
        for k in [
            "endpoint",
            "region",
            "version_id",
            "anonymous",
            "path_style",
        ] {
            obj.remove(k);
        }
        if obj.is_empty() {
            return Ok(String::new());
        }
    }
    Ok(v.to_string())
}

/// Parse an `s3://<bucket>/<key...>` URI. Rejects malformed inputs
/// and empty keys (`s3://only-bucket`) — matching parse_bucket_key
/// in duckdb-cache/backends/cloud.rs.
fn parse_s3_uri(uri: &str) -> Result<(String, String), String> {
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| format!("cache s3 backend: not an s3:// URL: {uri}"))?;
    let (bucket, key) = match rest.split_once('/') {
        Some((b, k)) => (b, k),
        None => (rest, ""),
    };
    if bucket.is_empty() {
        return Err(format!("cache s3 backend: no bucket in {uri}"));
    }
    if key.is_empty() {
        return Err(format!("cache s3 backend: URI has no object key: {uri}"));
    }
    Ok((bucket.to_string(), key.to_string()))
}

fn s3_error_to_string(e: &s3_types::Error) -> String {
    match e {
        s3_types::Error::AccessDenied => "access-denied".to_string(),
        s3_types::Error::NoSuchBucket => "no-such-bucket".to_string(),
        s3_types::Error::NoSuchKey => "no-such-key".to_string(),
        s3_types::Error::InvalidBucketName => "invalid-bucket-name".to_string(),
        s3_types::Error::InvalidRequest(m) => format!("invalid-request: {m}"),
        s3_types::Error::NetworkError(m) => format!("network-error: {m}"),
        s3_types::Error::ParseError(m) => format!("parse-error: {m}"),
        s3_types::Error::Internal(m) => format!("internal: {m}"),
    }
}

fn resolve_region(p: &S3Params) -> String {
    if let Some(r) = &p.region {
        return r.clone();
    }
    if let Ok(r) = std::env::var("AWS_REGION") {
        if !r.is_empty() {
            return r;
        }
    }
    if let Ok(r) = std::env::var("AWS_DEFAULT_REGION") {
        if !r.is_empty() {
            return r;
        }
    }
    String::from("us-east-1")
}

fn resolve_endpoint(p: &S3Params, region: &str) -> String {
    if let Some(e) = &p.endpoint {
        return e.clone();
    }
    if let Ok(e) = std::env::var("AWS_ENDPOINT_URL") {
        if !e.is_empty() {
            return e;
        }
    }
    if region == "us-east-1" {
        String::from("https://s3.amazonaws.com")
    } else {
        format!("https://s3.{region}.amazonaws.com")
    }
}

fn resolve_credentials(p: &S3Params) -> s3_types::Credentials {
    if p.anonymous {
        // s3-wasm treats empty access-key-id + empty secret as
        // "anonymous" -- the base client skips the SigV4 header when
        // the access key is empty. This matches the object_store
        // `.with_skip_signature(true)` semantic on the native side.
        return s3_types::Credentials {
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: None,
        };
    }
    let ak = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
    let sk = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
    let token = std::env::var("AWS_SESSION_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    s3_types::Credentials {
        access_key_id: ak,
        secret_access_key: sk,
        session_token: token,
    }
}

/// Head-then-get flow against `s3-base`. Mirrors
/// `duckdb-cache/backends/cloud.rs::fetch_via_head`:
///
///   1. `head-object` — cheap metadata probe. If the returned ETag
///      matches the catalog's cached etag and no `version_id` pin is
///      requested, skip the body download and re-touch the catalog
///      row (`revalidate`-style behaviour).
///   2. Otherwise `get-object`, hash the body, publish to the
///      content-addressed store, and upsert the catalog row.
fn resolve_s3(cfg: &Config, params: S3Params, uri: &str, scheme: &str) -> Result<String, String> {
    let (bucket, key) = parse_s3_uri(uri)?;
    let region = resolve_region(&params);
    let endpoint_url = resolve_endpoint(&params, &region);
    let creds = resolve_credentials(&params);

    let root = cache_root()?;
    ensure_dirs(&root)?;
    let existing = lookup(&cfg.name, uri)?;

    // Offline: never touch the network.
    if cfg.policy == Policy::Offline {
        return match existing {
            Some(e) => {
                enforce_sha_pin(cfg, &e.content_hash, uri)?;
                Ok(path_to_file_uri(std::path::Path::new(&e.resolved_path)))
            }
            None => Err(format!(
                "cache: policy \"offline\" and no cached entry for {uri}"
            )),
        };
    }

    // Immutable + pinned-sha + blob on disk -> done, no network.
    if cfg.policy == Policy::Immutable {
        if let Some(expected) = &cfg.sha256 {
            let pinned = blob_path(&root, expected);
            if pinned.is_file() {
                if existing.is_none() {
                    let now = compute_now();
                    let len = std::fs::metadata(&pinned).ok().map(|m| m.len() as i64);
                    upsert(
                        &cfg.name,
                        uri,
                        scheme,
                        None,
                        None,
                        expected,
                        len,
                        &pinned.to_string_lossy(),
                        &now,
                        None,
                    )?;
                }
                return Ok(path_to_file_uri(&pinned));
            }
        }
    }

    // v0.2 lock (advisory) — matches the http/https path so concurrent
    // s3 fetches of the same URI coalesce to one download.
    let lock_path = root
        .join("locks")
        .join(format!("{}.lock", cache_core::sha256_hex(uri.as_bytes())));
    let _uri_lock: Option<file_lock::LockHandle> =
        match file_lock::acquire_exclusive(&lock_path.to_string_lossy()) {
            Ok(handle) => {
                if let Some(now_cached) = lookup(&cfg.name, uri)? {
                    enforce_sha_pin(cfg, &now_cached.content_hash, uri)?;
                    return Ok(path_to_file_uri(std::path::Path::new(
                        &now_cached.resolved_path,
                    )));
                }
                Some(handle)
            }
            Err(_) => None,
        };

    let endpoint = s3_types::EndpointConfig {
        url: endpoint_url,
        region: region.clone(),
        // MinIO / LocalStack / R2-with-custom-endpoint use path-style
        // by default (bucket in the path, not the subdomain). When
        // `endpoint` is set and `path_style` isn't explicitly false,
        // prefer path-style so non-AWS endpoints Just Work.
        path_style: params.path_style || params.endpoint.is_some(),
    };

    // Step 1: HEAD. If the ETag matches the catalog + no version pin,
    // short-circuit before pulling bytes.
    let head_res = s3_base::head_object(&endpoint, &creds, &bucket, &key);
    let (head_etag, head_last_modified) = match head_res {
        Ok(h) => {
            let etag = h.metadata.etag.clone();
            let last_modified = h.metadata.last_modified.map(|s| s.to_string());
            (etag, last_modified)
        }
        // A HEAD failure is informational — fall through to GET so the
        // real error surfaces on the byte transfer (matches how the
        // native's object_store surface reports errors on `get_opts`).
        Err(_) => (None, None),
    };
    if params.version_id.is_none() {
        if let (Some(cur), Some(existing_row)) = (&head_etag, existing.as_ref()) {
            if existing_row.etag.as_deref() == Some(cur.as_str()) {
                // Unchanged -- re-touch validated_at and return the
                // cached blob without downloading.
                let now = compute_now();
                upsert(
                    &cfg.name,
                    uri,
                    scheme,
                    head_etag.as_deref(),
                    head_last_modified.as_deref(),
                    &existing_row.content_hash,
                    None,
                    &existing_row.resolved_path,
                    &now,
                    None,
                )?;
                enforce_sha_pin(cfg, &existing_row.content_hash, uri)?;
                return Ok(path_to_file_uri(std::path::Path::new(
                    &existing_row.resolved_path,
                )));
            }
        }
    }

    // Step 2: GET. `version_id` isn't threaded through s3-wasm's
    // `get-object-options` yet -- when the caller pins a version,
    // surface a clear error rather than silently returning the
    // current version's bytes.
    if params.version_id.is_some() {
        return Err(String::from(
            "cache s3 backend: version_id is not yet supported by the s3-wasm surface \
             (follow-up: extend s3-wasm's get-object-options with a version field).",
        ));
    }
    let got = s3_base::get_object(&endpoint, &creds, &bucket, &key, None)
        .map_err(|e| format!("cache s3 backend: get {uri}: {}", s3_error_to_string(&e)))?;
    let body_vec: Vec<u8> = got.body.to_vec();
    let now = compute_now();
    let hash = cache_core::sha256_hex(&body_vec);
    if let Some(expected) = &cfg.sha256 {
        if &hash != expected {
            return Err(format!(
                "cache: sha256 mismatch for {uri}: expected {expected}, got {hash}"
            ));
        }
    }
    let content_length = body_vec.len() as i64;
    let final_path = blob_path(&root, &hash);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cache staging: creating {}: {e}", parent.display()))?;
    }
    if !final_path.exists() {
        let tmp_name = format!("{}.{}", hash, TMP_COUNTER.fetch_add(1, Ordering::Relaxed));
        let tmp_path = root.join("tmp").join(&tmp_name);
        std::fs::write(&tmp_path, &body_vec)
            .map_err(|e| format!("cache staging: writing tmp: {e}"))?;
        match std::fs::rename(&tmp_path, &final_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp_path);
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!(
                    "cache staging: renaming to {}: {err}",
                    final_path.display()
                ));
            }
        }
    }
    let etag = got.metadata.etag.clone().or(head_etag);
    let last_modified = got
        .metadata
        .last_modified
        .map(|s| s.to_string())
        .or(head_last_modified);
    upsert(
        &cfg.name,
        uri,
        scheme,
        etag.as_deref(),
        last_modified.as_deref(),
        &hash,
        Some(content_length),
        &final_path.to_string_lossy(),
        &now,
        None,
    )?;
    Ok(path_to_file_uri(&final_path))
}

// ---------------------------------------------------------------------------
// azure backend (composed via `component:azure-wasm/{blob-base,...}`).
//
// Same head-then-get flow as the s3 backend, adapted to Azure's REST
// surface. Azure Blob Storage differs from S3 in a few ways that shape
// the parameter surface:
//
//   * NO `region` concept. The account URL bakes in the region.
//   * NO `version_id` — Azure has versioning, but the surface here is
//     not yet threaded through blob-base's get-blob-options.
//   * `endpoint` is optional and either:
//       - the full base URL (Azure public / sovereign clouds:
//         `https://<account>.blob.core.windows.net`)
//       - Azurite emulator: `http://127.0.0.1:10000/<account>` — signer
//         must set `emulator: true` (canonical resource is double-
//         prefixed with the account name).
//     Default: derived from `account` -> the public cloud URL.
//   * Auth: SharedKey (default; from `shared_key` or `AZURE_STORAGE_KEY`
//     env) vs SAS (`sas_token`) vs anonymous (`"anonymous": true`,
//     for public containers).
//
// URI shape: `az://<container>/<blob>` OR `azure://<container>/<blob>`
// — both are canonicalized on parse.
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
struct AzureParams {
    endpoint: Option<String>,
    account: Option<String>,
    shared_key: Option<String>,
    sas_token: Option<String>,
    anonymous: bool,
}

impl AzureParams {
    /// Parse the azure-specific fields off a JSON blob (leniently — the
    /// full JSON is also handed to cache-core's strict parser for the
    /// name/policy/sha256/ttl fields). Empty / missing config -> defaults.
    fn from_json(s: &str) -> Result<Self, String> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("cache azure config: {e}"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| String::from("cache azure config: expected a JSON object"))?;
        let mut out = Self::default();
        if let Some(ep) = obj.get("endpoint") {
            out.endpoint = Some(
                ep.as_str()
                    .ok_or_else(|| String::from("cache azure config: endpoint must be a string"))?
                    .to_string(),
            );
        }
        if let Some(acct) = obj.get("account") {
            out.account = Some(
                acct.as_str()
                    .ok_or_else(|| String::from("cache azure config: account must be a string"))?
                    .to_string(),
            );
        }
        if let Some(sk) = obj.get("shared_key") {
            out.shared_key = Some(
                sk.as_str()
                    .ok_or_else(|| String::from("cache azure config: shared_key must be a string"))?
                    .to_string(),
            );
        }
        if let Some(sas) = obj.get("sas_token") {
            out.sas_token = Some(
                sas.as_str()
                    .ok_or_else(|| String::from("cache azure config: sas_token must be a string"))?
                    .to_string(),
            );
        }
        if let Some(anon) = obj.get("anonymous") {
            out.anonymous = anon
                .as_bool()
                .ok_or_else(|| String::from("cache azure config: anonymous must be a boolean"))?;
        }
        Ok(out)
    }
}

/// Strip the azure-specific keys from a config JSON blob so cache-core's
/// strict parser can still handle the shared knobs (name, policy, sha256,
/// ttl). Returns an empty string when nothing remains, which cache-core
/// treats as `Config::default()`.
fn strip_azure_keys(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        return Ok(String::new());
    }
    let mut v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("cache azure config: {e}"))?;
    if let Some(obj) = v.as_object_mut() {
        for k in [
            "endpoint",
            "account",
            "shared_key",
            "sas_token",
            "anonymous",
        ] {
            obj.remove(k);
        }
        if obj.is_empty() {
            return Ok(String::new());
        }
    }
    Ok(v.to_string())
}

/// Parse an `az://<container>/<blob>` or `azure://<container>/<blob>`
/// URI. Rejects malformed inputs and empty blob names.
fn parse_azure_uri(uri: &str) -> Result<(String, String), String> {
    let rest = if let Some(r) = uri.strip_prefix("az://") {
        r
    } else if let Some(r) = uri.strip_prefix("azure://") {
        r
    } else {
        return Err(format!(
            "cache azure backend: not an az://|azure:// URL: {uri}"
        ));
    };
    let (container, blob) = match rest.split_once('/') {
        Some((c, b)) => (c, b),
        None => (rest, ""),
    };
    if container.is_empty() {
        return Err(format!("cache azure backend: no container in {uri}"));
    }
    if blob.is_empty() {
        return Err(format!("cache azure backend: URI has no blob name: {uri}"));
    }
    Ok((container.to_string(), blob.to_string()))
}

fn azure_error_to_string(e: &blob_types::Error) -> String {
    match e {
        blob_types::Error::AccessDenied => "access-denied".to_string(),
        blob_types::Error::NoSuchContainer => "no-such-container".to_string(),
        blob_types::Error::NoSuchBlob => "no-such-blob".to_string(),
        blob_types::Error::InvalidContainerName => "invalid-container-name".to_string(),
        blob_types::Error::InvalidRequest(m) => format!("invalid-request: {m}"),
        blob_types::Error::NetworkError(m) => format!("network-error: {m}"),
        blob_types::Error::ParseError(m) => format!("parse-error: {m}"),
        blob_types::Error::Internal(m) => format!("internal: {m}"),
    }
}

/// Resolve the storage account name from (config, env). Required for
/// SharedKey auth AND for deriving the default endpoint URL.
fn resolve_azure_account(p: &AzureParams) -> Option<String> {
    if let Some(a) = &p.account {
        if !a.is_empty() {
            return Some(a.clone());
        }
    }
    if let Ok(a) = std::env::var("AZURE_STORAGE_ACCOUNT") {
        if !a.is_empty() {
            return Some(a);
        }
    }
    None
}

/// Build the endpoint config (URL + emulator flag) from params and the
/// resolved account name. Detects the Azurite emulator by the shape of
/// the endpoint URL so the signer can canonicalize the resource with
/// the account double-prefix Azurite requires.
fn resolve_azure_endpoint(p: &AzureParams, account: &str) -> blob_types::EndpointConfig {
    let (url, emulator) = if let Some(e) = &p.endpoint {
        // Heuristic: any endpoint pointing at a loopback / dev host
        // is an emulator. Matches the Azurite default binding and the
        // devstoreaccount1 quickstart URL.
        let lc = e.to_ascii_lowercase();
        let is_emu = lc.contains("127.0.0.1")
            || lc.contains("localhost")
            || lc.contains("azurite")
            || lc.contains("devstoreaccount");
        (e.clone(), is_emu)
    } else if let Ok(e) = std::env::var("AZURE_STORAGE_ENDPOINT_URL") {
        if !e.is_empty() {
            let lc = e.to_ascii_lowercase();
            let is_emu = lc.contains("127.0.0.1")
                || lc.contains("localhost")
                || lc.contains("azurite")
                || lc.contains("devstoreaccount");
            (e, is_emu)
        } else {
            let suffix = std::env::var("AZURE_STORAGE_ENDPOINT_SUFFIX")
                .unwrap_or_else(|_| String::from("core.windows.net"));
            (format!("https://{account}.blob.{suffix}"), false)
        }
    } else {
        let suffix = std::env::var("AZURE_STORAGE_ENDPOINT_SUFFIX")
            .unwrap_or_else(|_| String::from("core.windows.net"));
        (format!("https://{account}.blob.{suffix}"), false)
    };
    blob_types::EndpointConfig { url, emulator }
}

/// Assemble the credentials record. Auth precedence:
///   1. anonymous (skip both key + SAS)
///   2. SAS token (config field OR AZURE_STORAGE_SAS_TOKEN env)
///   3. SharedKey (config field OR AZURE_STORAGE_KEY env)
fn resolve_azure_credentials(p: &AzureParams, account: &str) -> blob_types::Credentials {
    if p.anonymous {
        return blob_types::Credentials {
            account: account.to_string(),
            shared_key: None,
            sas_token: None,
        };
    }
    // Prefer SAS when explicitly supplied, else fall back to shared key.
    let sas_from_env = std::env::var("AZURE_STORAGE_SAS_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let sas_token = p.sas_token.clone().or(sas_from_env);
    if sas_token.is_some() {
        return blob_types::Credentials {
            account: account.to_string(),
            shared_key: None,
            sas_token,
        };
    }
    let key_from_env = std::env::var("AZURE_STORAGE_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let shared_key = p.shared_key.clone().or(key_from_env);
    blob_types::Credentials {
        account: account.to_string(),
        shared_key,
        sas_token: None,
    }
}

/// Head-then-get flow against `blob-base`. Mirrors resolve_s3's structure:
///
///   1. `head-blob` — cheap metadata probe. If the returned ETag matches
///      the catalog's cached etag, skip the body download and re-touch
///      the catalog row.
///   2. Otherwise `get-blob`, hash the body, publish to the content-
///      addressed store, and upsert the catalog row.
fn resolve_azure(
    cfg: &Config,
    params: AzureParams,
    uri: &str,
    scheme: &str,
) -> Result<String, String> {
    let (container, blob_name) = parse_azure_uri(uri)?;
    let account = resolve_azure_account(&params).ok_or_else(|| {
        String::from(
            "cache azure backend: no storage account (set `account` in the config \
             JSON or the AZURE_STORAGE_ACCOUNT env var)",
        )
    })?;
    let endpoint = resolve_azure_endpoint(&params, &account);
    let creds = resolve_azure_credentials(&params, &account);

    let root = cache_root()?;
    ensure_dirs(&root)?;
    let existing = lookup(&cfg.name, uri)?;

    // Offline: never touch the network.
    if cfg.policy == Policy::Offline {
        return match existing {
            Some(e) => {
                enforce_sha_pin(cfg, &e.content_hash, uri)?;
                Ok(path_to_file_uri(std::path::Path::new(&e.resolved_path)))
            }
            None => Err(format!(
                "cache: policy \"offline\" and no cached entry for {uri}"
            )),
        };
    }

    // Immutable + pinned-sha + blob on disk -> done, no network.
    if cfg.policy == Policy::Immutable {
        if let Some(expected) = &cfg.sha256 {
            let pinned = blob_path(&root, expected);
            if pinned.is_file() {
                if existing.is_none() {
                    let now = compute_now();
                    let len = std::fs::metadata(&pinned).ok().map(|m| m.len() as i64);
                    upsert(
                        &cfg.name,
                        uri,
                        scheme,
                        None,
                        None,
                        expected,
                        len,
                        &pinned.to_string_lossy(),
                        &now,
                        None,
                    )?;
                }
                return Ok(path_to_file_uri(&pinned));
            }
        }
    }

    // v0.2 lock (advisory) — matches the http/https and s3 paths so
    // concurrent azure fetches of the same URI coalesce to one download.
    let lock_path = root
        .join("locks")
        .join(format!("{}.lock", cache_core::sha256_hex(uri.as_bytes())));
    let _uri_lock: Option<file_lock::LockHandle> =
        match file_lock::acquire_exclusive(&lock_path.to_string_lossy()) {
            Ok(handle) => {
                if let Some(now_cached) = lookup(&cfg.name, uri)? {
                    enforce_sha_pin(cfg, &now_cached.content_hash, uri)?;
                    return Ok(path_to_file_uri(std::path::Path::new(
                        &now_cached.resolved_path,
                    )));
                }
                Some(handle)
            }
            Err(_) => None,
        };

    // Step 1: HEAD. If the ETag matches the catalog, short-circuit.
    let head_res = blob_base::head_blob(&endpoint, &creds, &container, &blob_name);
    let (head_etag, head_last_modified) = match head_res {
        Ok(h) => {
            let etag = h.metadata.etag.clone();
            let last_modified = h.metadata.last_modified.map(|s| s.to_string());
            (etag, last_modified)
        }
        // A HEAD failure is informational — fall through to GET so the
        // real error surfaces on the byte transfer.
        Err(_) => (None, None),
    };
    if let (Some(cur), Some(existing_row)) = (&head_etag, existing.as_ref()) {
        if existing_row.etag.as_deref() == Some(cur.as_str()) {
            let now = compute_now();
            upsert(
                &cfg.name,
                uri,
                scheme,
                head_etag.as_deref(),
                head_last_modified.as_deref(),
                &existing_row.content_hash,
                None,
                &existing_row.resolved_path,
                &now,
                None,
            )?;
            enforce_sha_pin(cfg, &existing_row.content_hash, uri)?;
            return Ok(path_to_file_uri(std::path::Path::new(
                &existing_row.resolved_path,
            )));
        }
    }

    // Step 2: GET.
    let got =
        blob_base::get_blob(&endpoint, &creds, &container, &blob_name, None).map_err(|e| {
            format!(
                "cache azure backend: get {uri}: {}",
                azure_error_to_string(&e)
            )
        })?;
    let body_vec: Vec<u8> = got.body.to_vec();
    let now = compute_now();
    let hash = cache_core::sha256_hex(&body_vec);
    if let Some(expected) = &cfg.sha256 {
        if &hash != expected {
            return Err(format!(
                "cache: sha256 mismatch for {uri}: expected {expected}, got {hash}"
            ));
        }
    }
    let content_length = body_vec.len() as i64;
    let final_path = blob_path(&root, &hash);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cache staging: creating {}: {e}", parent.display()))?;
    }
    if !final_path.exists() {
        let tmp_name = format!("{}.{}", hash, TMP_COUNTER.fetch_add(1, Ordering::Relaxed));
        let tmp_path = root.join("tmp").join(&tmp_name);
        std::fs::write(&tmp_path, &body_vec)
            .map_err(|e| format!("cache staging: writing tmp: {e}"))?;
        match std::fs::rename(&tmp_path, &final_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp_path);
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!(
                    "cache staging: renaming to {}: {err}",
                    final_path.display()
                ));
            }
        }
    }
    let etag = got.metadata.etag.clone().or(head_etag);
    let last_modified = got
        .metadata
        .last_modified
        .map(|s| s.to_string())
        .or(head_last_modified);
    upsert(
        &cfg.name,
        uri,
        scheme,
        etag.as_deref(),
        last_modified.as_deref(),
        &hash,
        Some(content_length),
        &final_path.to_string_lossy(),
        &now,
        None,
    )?;
    Ok(path_to_file_uri(&final_path))
}

// ---------------------------------------------------------------------------
// gcs backend (composed via `component:gcs-wasm/{blob-base,blob-oauth,...}`).
//
// Same head-then-get shape as the s3 and azure backends, adapted to
// GCS's REST surface. Key differences from the sibling backends:
//
//   * NO `region` concept. The default endpoint is
//     https://storage.googleapis.com and rarely needs overriding.
//   * NO SigV4 / SharedKey signing. Auth is one of:
//       - service-account JSON (RS256-JWT -> OAuth2 access token).
//         gcs-wasm caches the minted bearer internally; the resolver
//         additionally caches it on this side so subsequent calls
//         skip the JWT sign + POST entirely.
//       - a pre-minted access token (from `gcloud auth print-access-token`
//         or workload identity fed in from the host).
//       - anonymous (public buckets).
//   * `head-blob` / `get-blob` now return a parsed `object-metadata`
//     struct (parallel to s3-wasm's `object-metadata` and azure-wasm's
//     `blob-metadata`), so the resolver harvests ETag / Last-Modified
//     via `.metadata` — one uniform code path across the three cloud
//     backends. Raw `headers` are still returned as an escape hatch.
//
// URI shape: `gs://<bucket>/<key>`.
//
// Auth resolution order (mirrors the README):
//   1. `"anonymous": true` — skip everything.
//   2. `access_token` (config field) — use verbatim.
//   3. `service_account_json` (config field, inline JSON blob).
//   4. `service_account_path` (config field, path to SA JSON on disk).
//   5. `GOOGLE_APPLICATION_CREDENTIALS` env var (path to SA JSON).
// If none resolve, the request fails with a clear "no GCS credentials
// found" error unless the URI actually can be served anonymously
// (that path requires explicit `"anonymous": true` to avoid silent
// fallthrough on typo'd credential paths).
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
struct GcsParams {
    endpoint: Option<String>,
    service_account_json: Option<String>,
    service_account_path: Option<String>,
    access_token: Option<String>,
    anonymous: bool,
}

impl GcsParams {
    /// Parse the gcs-specific fields off a JSON blob (leniently — the
    /// full JSON is also handed to cache-core's strict parser for the
    /// name/policy/sha256/ttl fields). Empty / missing config -> defaults.
    fn from_json(s: &str) -> Result<Self, String> {
        if s.trim().is_empty() {
            return Ok(Self::default());
        }
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("cache gcs config: {e}"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| String::from("cache gcs config: expected a JSON object"))?;
        let mut out = Self::default();
        if let Some(ep) = obj.get("endpoint") {
            out.endpoint = Some(
                ep.as_str()
                    .ok_or_else(|| String::from("cache gcs config: endpoint must be a string"))?
                    .to_string(),
            );
        }
        if let Some(saj) = obj.get("service_account_json") {
            // Accept either a stringified JSON blob or a nested JSON
            // object (the latter is the natural shape when the caller
            // has already parsed the SA key). We normalise to a string
            // so the downstream JSON parse is uniform.
            let s = match saj {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Object(_) => saj.to_string(),
                _ => {
                    return Err(String::from(
                        "cache gcs config: service_account_json must be a string or object",
                    ));
                }
            };
            out.service_account_json = Some(s);
        }
        if let Some(sap) = obj.get("service_account_path") {
            out.service_account_path = Some(
                sap.as_str()
                    .ok_or_else(|| {
                        String::from("cache gcs config: service_account_path must be a string")
                    })?
                    .to_string(),
            );
        }
        if let Some(tok) = obj.get("access_token") {
            out.access_token = Some(
                tok.as_str()
                    .ok_or_else(|| String::from("cache gcs config: access_token must be a string"))?
                    .to_string(),
            );
        }
        if let Some(anon) = obj.get("anonymous") {
            out.anonymous = anon
                .as_bool()
                .ok_or_else(|| String::from("cache gcs config: anonymous must be a boolean"))?;
        }
        Ok(out)
    }
}

/// Strip the gcs-specific keys from a config JSON blob so cache-core's
/// strict parser can still handle the shared knobs (name, policy,
/// sha256, ttl). Returns an empty string when nothing remains, which
/// cache-core treats as `Config::default()`.
fn strip_gcs_keys(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        return Ok(String::new());
    }
    let mut v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("cache gcs config: {e}"))?;
    if let Some(obj) = v.as_object_mut() {
        for k in [
            "endpoint",
            "service_account_json",
            "service_account_path",
            "access_token",
            "anonymous",
        ] {
            obj.remove(k);
        }
        if obj.is_empty() {
            return Ok(String::new());
        }
    }
    Ok(v.to_string())
}

/// Parse a `gs://<bucket>/<key...>` URI. Rejects malformed inputs
/// and empty keys (`gs://only-bucket`).
fn parse_gcs_uri(uri: &str) -> Result<(String, String), String> {
    let rest = uri
        .strip_prefix("gs://")
        .ok_or_else(|| format!("cache gcs backend: not a gs:// URL: {uri}"))?;
    let (bucket, key) = match rest.split_once('/') {
        Some((b, k)) => (b, k),
        None => (rest, ""),
    };
    if bucket.is_empty() {
        return Err(format!("cache gcs backend: no bucket in {uri}"));
    }
    if key.is_empty() {
        return Err(format!("cache gcs backend: URI has no object key: {uri}"));
    }
    Ok((bucket.to_string(), key.to_string()))
}

fn gcs_error_to_string(e: &gcs_blob_types::Error) -> String {
    match e {
        gcs_blob_types::Error::AccessDenied => "access-denied".to_string(),
        gcs_blob_types::Error::NoSuchBucket => "no-such-bucket".to_string(),
        gcs_blob_types::Error::NoSuchObject => "no-such-object".to_string(),
        gcs_blob_types::Error::InvalidRequest(m) => format!("invalid-request: {m}"),
        gcs_blob_types::Error::NetworkError(m) => format!("network-error: {m}"),
        gcs_blob_types::Error::ParseError(m) => format!("parse-error: {m}"),
        gcs_blob_types::Error::Internal(m) => format!("internal: {m}"),
    }
}

fn resolve_gcs_endpoint(p: &GcsParams) -> gcs_blob_types::EndpointConfig {
    let url = if let Some(e) = &p.endpoint {
        e.clone()
    } else if let Ok(e) = std::env::var("GOOGLE_STORAGE_ENDPOINT_URL") {
        if e.is_empty() {
            String::from("https://storage.googleapis.com")
        } else {
            e
        }
    } else {
        String::from("https://storage.googleapis.com")
    };
    gcs_blob_types::EndpointConfig { url }
}

/// A minted OAuth2 bearer cached across resolver calls. Keyed by the
/// service-account `client_email` so multiple SAs coexist correctly.
/// Refreshed 60 s before expiry to keep in-flight requests off the
/// edge (matches the safety margin gcs-wasm applies internally when
/// it caches).
struct CachedToken {
    token: String,
    expires_at: u64,
}

fn gcs_token_cache() -> &'static Mutex<HashMap<String, CachedToken>> {
    static T: OnceLock<Mutex<HashMap<String, CachedToken>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the raw service-account JSON string from (inline config,
/// path config, GOOGLE_APPLICATION_CREDENTIALS env). Returns None
/// when the caller isn't using the SA flow.
fn load_gcs_service_account_json(p: &GcsParams) -> Result<Option<String>, String> {
    if let Some(s) = &p.service_account_json {
        return Ok(Some(s.clone()));
    }
    if let Some(path) = &p.service_account_path {
        let s = std::fs::read_to_string(path)
            .map_err(|e| format!("cache gcs backend: reading service_account_path {path}: {e}"))?;
        return Ok(Some(s));
    }
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        if !path.is_empty() {
            let s = std::fs::read_to_string(&path).map_err(|e| {
                format!("cache gcs backend: reading GOOGLE_APPLICATION_CREDENTIALS={path}: {e}")
            })?;
            return Ok(Some(s));
        }
    }
    Ok(None)
}

/// Parse an SA JSON blob into a gcs-wasm `service-account` record.
/// Extracts `client_email`, `private_key`, `token_uri` (optional).
/// Scopes default to devstorage.read_only when the caller doesn't
/// specify (matches gcs-wasm's own default; cache reads only need
/// read scope).
fn parse_gcs_service_account(sa_json: &str) -> Result<gcs_blob_types::ServiceAccount, String> {
    let v: serde_json::Value = serde_json::from_str(sa_json)
        .map_err(|e| format!("cache gcs backend: parsing service-account JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| String::from("cache gcs backend: service-account JSON must be an object"))?;
    let email = obj
        .get("client_email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            String::from("cache gcs backend: service-account JSON missing client_email")
        })?
        .to_string();
    let private_key_pem = obj
        .get("private_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| String::from("cache gcs backend: service-account JSON missing private_key"))?
        .to_string();
    let token_uri = obj
        .get("token_uri")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(gcs_blob_types::ServiceAccount {
        email,
        private_key_pem,
        token_uri,
        // gcs-wasm defaults an empty scopes list to devstorage.read_only.
        scopes: WitVec::new().into(),
    })
}

/// Mint (or reuse) a bearer for the given service-account. Cached
/// per-email; refreshed 60 s before expiry.
fn mint_or_cached_gcs_token(
    sa: &gcs_blob_types::ServiceAccount,
) -> Result<gcs_blob_types::AccessToken, String> {
    let now = now_epoch_secs();
    {
        let cache = gcs_token_cache().lock().expect("poisoned");
        if let Some(entry) = cache.get(&sa.email) {
            if entry.expires_at > now + 60 {
                return Ok(gcs_blob_types::AccessToken {
                    token: entry.token.clone(),
                    expires_at: entry.expires_at,
                });
            }
        }
    }
    let minted = gcs_blob_oauth::mint_access_token(sa).map_err(|e| {
        format!(
            "cache gcs backend: mint access token: {}",
            gcs_error_to_string(&e)
        )
    })?;
    {
        let mut cache = gcs_token_cache().lock().expect("poisoned");
        cache.insert(
            sa.email.clone(),
            CachedToken {
                token: minted.token.clone(),
                expires_at: minted.expires_at,
            },
        );
    }
    Ok(minted)
}

/// Assemble the credentials record. Precedence documented on the module.
fn resolve_gcs_credentials(p: &GcsParams) -> Result<gcs_blob_types::Credentials, String> {
    if p.anonymous {
        return Ok(gcs_blob_types::Credentials::Anonymous);
    }
    if let Some(tok) = &p.access_token {
        if !tok.is_empty() {
            return Ok(gcs_blob_types::Credentials::AccessToken(
                gcs_blob_types::AccessToken {
                    token: tok.clone(),
                    // Caller opted in to raw token — we don't know its
                    // expiry, so mark it as long-lived. gcs-wasm doesn't
                    // consult this field, it just attaches the bearer.
                    expires_at: u64::MAX,
                },
            ));
        }
    }
    if let Some(sa_json) = load_gcs_service_account_json(p)? {
        let sa = parse_gcs_service_account(&sa_json)?;
        let tok = mint_or_cached_gcs_token(&sa)?;
        return Ok(gcs_blob_types::Credentials::AccessToken(tok));
    }
    Err(String::from(
        "cache gcs backend: no credentials resolved. Set one of: \
         config `anonymous: true`, config `access_token`, config \
         `service_account_json` / `service_account_path`, or env \
         `GOOGLE_APPLICATION_CREDENTIALS` (path to SA JSON).",
    ))
}

/// Head-then-get flow against `gcs_blob_base`. Mirrors resolve_s3 /
/// resolve_azure:
///
///   1. `head-blob` — cheap metadata probe. If the returned ETag
///      matches the catalog's cached etag, skip the body download
///      and re-touch the catalog row.
///   2. Otherwise `get-blob`, hash the body, publish to the
///      content-addressed store, and upsert the catalog row.
///
/// Since gcs-wasm now returns a parsed `object-metadata` on head/get
/// (parallel to s3-wasm's `object-metadata` and azure-wasm's
/// `blob-metadata`), the resolver just reads `.metadata.etag` /
/// `.metadata.last_modified` — no more hand-parsing of the header
/// list — leaving the three backend harvest paths structurally
/// identical.
fn resolve_gcs(cfg: &Config, params: GcsParams, uri: &str, scheme: &str) -> Result<String, String> {
    let (bucket, key) = parse_gcs_uri(uri)?;
    let endpoint = resolve_gcs_endpoint(&params);
    let creds = resolve_gcs_credentials(&params)?;

    let root = cache_root()?;
    ensure_dirs(&root)?;
    let existing = lookup(&cfg.name, uri)?;

    // Offline: never touch the network.
    if cfg.policy == Policy::Offline {
        return match existing {
            Some(e) => {
                enforce_sha_pin(cfg, &e.content_hash, uri)?;
                Ok(path_to_file_uri(std::path::Path::new(&e.resolved_path)))
            }
            None => Err(format!(
                "cache: policy \"offline\" and no cached entry for {uri}"
            )),
        };
    }

    // Immutable + pinned-sha + blob on disk -> done, no network.
    if cfg.policy == Policy::Immutable {
        if let Some(expected) = &cfg.sha256 {
            let pinned = blob_path(&root, expected);
            if pinned.is_file() {
                if existing.is_none() {
                    let now = compute_now();
                    let len = std::fs::metadata(&pinned).ok().map(|m| m.len() as i64);
                    upsert(
                        &cfg.name,
                        uri,
                        scheme,
                        None,
                        None,
                        expected,
                        len,
                        &pinned.to_string_lossy(),
                        &now,
                        None,
                    )?;
                }
                return Ok(path_to_file_uri(&pinned));
            }
        }
    }

    // v0.2 lock (advisory) — matches the other cloud backends.
    let lock_path = root
        .join("locks")
        .join(format!("{}.lock", cache_core::sha256_hex(uri.as_bytes())));
    let _uri_lock: Option<file_lock::LockHandle> =
        match file_lock::acquire_exclusive(&lock_path.to_string_lossy()) {
            Ok(handle) => {
                if let Some(now_cached) = lookup(&cfg.name, uri)? {
                    enforce_sha_pin(cfg, &now_cached.content_hash, uri)?;
                    return Ok(path_to_file_uri(std::path::Path::new(
                        &now_cached.resolved_path,
                    )));
                }
                Some(handle)
            }
            Err(_) => None,
        };

    // Step 1: HEAD. Consume the pre-parsed metadata struct that gcs-wasm now
    // emits — same shape s3-wasm and azure-wasm already ship. No more
    // header-list scanning on this side.
    let head_res = gcs_blob_base::head_blob(&endpoint, &creds, &bucket, &key);
    let (head_etag, head_last_modified) = match head_res {
        Ok(h) => match h.metadata {
            Some(m) => (m.etag, m.last_modified),
            None => (None, None),
        },
        // A HEAD failure is informational — fall through to GET so the
        // real error surfaces on the byte transfer.
        Err(_) => (None, None),
    };
    if let (Some(cur), Some(existing_row)) = (&head_etag, existing.as_ref()) {
        if existing_row.etag.as_deref() == Some(cur.as_str()) {
            let now = compute_now();
            upsert(
                &cfg.name,
                uri,
                scheme,
                head_etag.as_deref(),
                head_last_modified.as_deref(),
                &existing_row.content_hash,
                None,
                &existing_row.resolved_path,
                &now,
                None,
            )?;
            enforce_sha_pin(cfg, &existing_row.content_hash, uri)?;
            return Ok(path_to_file_uri(std::path::Path::new(
                &existing_row.resolved_path,
            )));
        }
    }

    // Step 2: GET.
    let got = gcs_blob_base::get_blob(&endpoint, &creds, &bucket, &key, None)
        .map_err(|e| format!("cache gcs backend: get {uri}: {}", gcs_error_to_string(&e)))?;
    // Non-2xx from the GET (only 200 is expected here; gcs-wasm surfaces
    // 304 through get-object-output.status, but the resolver didn't set
    // If-None-Match so 200 is the norm).
    if !(200..300).contains(&got.status) {
        return Err(format!(
            "cache gcs backend: get {uri} returned status {}",
            got.status
        ));
    }
    let body_vec: Vec<u8> = got.body.to_vec();
    let now = compute_now();
    let hash = cache_core::sha256_hex(&body_vec);
    if let Some(expected) = &cfg.sha256 {
        if &hash != expected {
            return Err(format!(
                "cache: sha256 mismatch for {uri}: expected {expected}, got {hash}"
            ));
        }
    }
    let content_length = body_vec.len() as i64;
    let final_path = blob_path(&root, &hash);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cache staging: creating {}: {e}", parent.display()))?;
    }
    if !final_path.exists() {
        let tmp_name = format!("{}.{}", hash, TMP_COUNTER.fetch_add(1, Ordering::Relaxed));
        let tmp_path = root.join("tmp").join(&tmp_name);
        std::fs::write(&tmp_path, &body_vec)
            .map_err(|e| format!("cache staging: writing tmp: {e}"))?;
        match std::fs::rename(&tmp_path, &final_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp_path);
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!(
                    "cache staging: renaming to {}: {err}",
                    final_path.display()
                ));
            }
        }
    }
    // Same shape as resolve_s3 / resolve_azure: prefer the GET response's
    // metadata, fall back to what HEAD reported.
    let (get_etag, get_last_modified) = match got.metadata {
        Some(m) => (m.etag, m.last_modified),
        None => (None, None),
    };
    let etag = get_etag.or(head_etag);
    let last_modified = get_last_modified.or(head_last_modified);
    upsert(
        &cfg.name,
        uri,
        scheme,
        etag.as_deref(),
        last_modified.as_deref(),
        &hash,
        Some(content_length),
        &final_path.to_string_lossy(),
        &now,
        None,
    )?;
    Ok(path_to_file_uri(&final_path))
}

// A tiny sanity call on the marker interface so composition catches
// a missing gcs-wasm plug at instantiation time rather than on the
// first `gs://` cache call. Called from `Guest::load`. This is
// specifically why `blob-anon` is imported — the interface exists to
// let composers verify wire-up, and this is where we exercise it.
fn probe_gcs_anon_wiring() -> bool {
    gcs_blob_anon::is_anonymous_supported()
}

fn enforce_sha_pin(cfg: &Config, cached_hash: &str, uri: &str) -> Result<(), String> {
    if let Some(expected) = &cfg.sha256 {
        if expected != cached_hash {
            return Err(format!(
                "cache: sha256 mismatch for {uri}: expected {expected}, cached {cached_hash}"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad scalar capability".into())),
    };
    // Both `cache-core` DECLS register under the SQL name `"cache"`;
    // DuckDB picks the overload by arity at bind time. The unique
    // identifier for the callback is the per-decl handle.
    for (idx, decl) in cache_core::Core::DECLS.iter().enumerate() {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        handles().lock().expect("poisoned").insert(handle, idx);
        let args: WitVec<runtime::Funcarg> = decl
            .args
            .iter()
            .map(|t| runtime::Funcarg {
                name: Some("value".into()),
                logical: ntype_to_logical(t),
            })
            .collect();
        let mut attributes = types::Funcflags::STATELESS;
        if decl.deterministic {
            attributes |= types::Funcflags::DETERMINISTIC;
        }
        let opts = runtime::Funcopts {
            description: Some(format!("cache scalar (arity {})", decl.args.len())),
            tags: vec!["cache".into()],
            attributes,
        };
        reg.register(
            cache_core::REGISTERED_NAME,
            &args,
            &ntype_to_logical(&decl.ret),
            runtime::ScalarCallback::new(handle),
            Some(&opts),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// guest::Guest (load / reconfigure / shutdown)
// ---------------------------------------------------------------------------

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // 1) Wire the resolver bridge into cache-core so scalar bodies
        //    can call `cache_core::resolve(cfg, uri)`.
        cache_core::install_resolver(resolver);

        // 2) Bootstrap the metadata catalog. Any failure here (bad
        //    preopen, sqlite:extension/spi not wired, etc.) surfaces as
        //    a load-time error via stderr — the cache is unusable
        //    without a catalog, so silently swallowing the failure
        //    only produced confusing "no such table" errors on the
        //    first cache() call.
        if let Err(e) = spi_bootstrap() {
            eprintln!("cache-component: spi_bootstrap failed: {e}");
        }

        // 3) Cheap probe of the gcs-wasm marker interface so a missing
        //    compose-time plug surfaces at extension load rather than
        //    on the first `gs://` cache call. The value is discarded;
        //    we only care that the imported function is present.
        let _ = probe_gcs_anon_wiring();

        // 4) Register the two cache overloads.
        register_scalars()?;

        Ok(types::Loadresult {
            name: <cache_core::Core as datalink_extcore::ExtCore>::NAME.into(),
            version: Some(<cache_core::Core as datalink_extcore::ExtCore>::VERSION.into()),
            requires: WitVec::new().into(),
        })
    }

    fn reconfigure(_keys: WitVec<WitString>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }

    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Per-row scalar entry point fanned out by the columnar bridge below.
// ---------------------------------------------------------------------------

fn cache_scalar(
    handle: u32,
    args: WitVec<types::Duckvalue>,
    _ctx: types::Invokeinfo,
) -> Result<types::Duckvalue, types::Duckerror> {
    let idx = handles()
        .lock()
        .expect("poisoned")
        .get(&handle)
        .copied()
        .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
    let decl = &cache_core::Core::DECLS[idx];
    let neutral: Vec<NeutralValue> = args.iter().map(to_neutral).collect();
    if matches!(decl.null_handling, NullHandling::Propagate) && neutral.iter().any(|v| v.is_null())
    {
        return Ok(types::Duckvalue::Null);
    }

    // s3:// intercept — cache-core's `Config::from_json` rejects any
    // s3-specific keys (deny_unknown_fields), so route the s3 flow
    // directly with a lenient parse for {endpoint, region,
    // version_id, anonymous, path_style}. The remaining common knobs
    // (name, policy, sha256, ttl) still go through cache-core's
    // strict parser after the s3-only keys are stripped.
    let (cfg_json_opt, uri_opt): (Option<&str>, Option<&str>) = match neutral.as_slice() {
        [NeutralValue::Text(u)] => (None, Some(u.as_str())),
        [NeutralValue::Text(j), NeutralValue::Text(u)] => (Some(j.as_str()), Some(u.as_str())),
        _ => (None, None),
    };
    if let Some(uri) = uri_opt {
        let lc = uri.trim_start().to_ascii_lowercase();
        if lc.starts_with("s3:") {
            let params = match cfg_json_opt {
                Some(j) => S3Params::from_json(j).map_err(duckerr)?,
                None => S3Params::default(),
            };
            let cfg = match cfg_json_opt {
                Some(j) => {
                    let stripped = strip_s3_keys(j).map_err(duckerr)?;
                    if stripped.is_empty() {
                        Config::default()
                    } else {
                        Config::from_json(&stripped).map_err(duckerr)?
                    }
                }
                None => Config::default(),
            };
            let out = resolve_s3(&cfg, params, uri, "s3").map_err(duckerr)?;
            return Ok(from_neutral(NeutralValue::Text(out)));
        }
        // az:// and azure:// intercept — same story as s3, but with
        // azure-specific config keys (endpoint / account / shared_key /
        // sas_token / anonymous). Canonicalize the scheme label to "az"
        // in the catalog regardless of which alias the caller used.
        if lc.starts_with("az:") || lc.starts_with("azure:") {
            let params = match cfg_json_opt {
                Some(j) => AzureParams::from_json(j).map_err(duckerr)?,
                None => AzureParams::default(),
            };
            let cfg = match cfg_json_opt {
                Some(j) => {
                    let stripped = strip_azure_keys(j).map_err(duckerr)?;
                    if stripped.is_empty() {
                        Config::default()
                    } else {
                        Config::from_json(&stripped).map_err(duckerr)?
                    }
                }
                None => Config::default(),
            };
            let out = resolve_azure(&cfg, params, uri, "az").map_err(duckerr)?;
            return Ok(from_neutral(NeutralValue::Text(out)));
        }
        // gs:// intercept — parallel to s3 and azure. Lenient parse of
        // the gcs-specific keys (endpoint / service_account_json /
        // service_account_path / access_token / anonymous), then the
        // remaining shared knobs pass through cache-core's strict
        // parser after those keys are stripped.
        if lc.starts_with("gs:") {
            let params = match cfg_json_opt {
                Some(j) => GcsParams::from_json(j).map_err(duckerr)?,
                None => GcsParams::default(),
            };
            let cfg = match cfg_json_opt {
                Some(j) => {
                    let stripped = strip_gcs_keys(j).map_err(duckerr)?;
                    if stripped.is_empty() {
                        Config::default()
                    } else {
                        Config::from_json(&stripped).map_err(duckerr)?
                    }
                }
                None => Config::default(),
            };
            let out = resolve_gcs(&cfg, params, uri, "gs").map_err(duckerr)?;
            return Ok(from_neutral(NeutralValue::Text(out)));
        }
    }

    let res = cache_core::Core::dispatch(idx, &neutral).map_err(duckerr)?;
    Ok(from_neutral(res))
}

datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = cache_scalar;
}

export!(Extension);
