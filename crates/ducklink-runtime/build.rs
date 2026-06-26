//! Compute the AUTHORITATIVE, content-addressed `duckdb:extension` contract
//! identity at build time and embed it as a `&'static str` const.
//!
//! The identity is a **witcanon digest** — `sha256("witcanon:1" || bytes)` over
//! the canonical `duckdb:extension` WIT-file bytes — the exact scheme exposed by
//! `compose-core::blobs::compute_wit_digest` in
//! `~/git/webassembly-component-orchestration` (SPEC §4.1). Keeping it
//! byte-identical means a ducklink contract digest interoperates with the
//! orchestration framework's blob identity.
//!
//! Every loadable component carries byte-identical *propagated* copies of these
//! WIT files (see `tooling/propagate-wit.py`), so hashing the canonical
//! `wit/duckdb-extension/*.wit` bytes is a stable, deterministic contract
//! identity that changes iff the WIT shape changes. The same bytes are hashed by
//! `tooling/gen-catalog.py` / `tooling/verify-catalog.py`, so the const the
//! runtime serves equals the digest the catalog records and enforces.
use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() {
    // Canonical contract dir, relative to this crate: <repo>/wit/duckdb-extension.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let wit_dir = manifest.join("..").join("..").join("wit").join("duckdb-extension");

    // Read every top-level `*.wit` file in DETERMINISTIC (sorted-by-filename)
    // order, concatenate the raw bytes, and witcanon-hash them.
    let mut files: Vec<PathBuf> = std::fs::read_dir(&wit_dir)
        .unwrap_or_else(|e| panic!("cannot read canonical WIT dir {}: {e}", wit_dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "wit").unwrap_or(false))
        .collect();
    files.sort();

    let mut bytes: Vec<u8> = Vec::new();
    for f in &files {
        // re-run the build if any canonical WIT file changes.
        println!("cargo:rerun-if-changed={}", f.display());
        bytes.extend_from_slice(&std::fs::read(f).expect("read canonical WIT file"));
    }
    println!("cargo:rerun-if-changed={}", wit_dir.display());

    // witcanon:1 scheme — byte-identical to compose-core::blobs::compute_wit_digest.
    let mut hasher = Sha256::new();
    hasher.update(b"witcanon:1");
    hasher.update(&bytes);
    let digest = hex::encode(hasher.finalize());

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("contract_digest.rs");
    std::fs::write(
        &out,
        format!("pub const CONTRACT_DIGEST: &str = \"{digest}\";\n"),
    )
    .expect("write contract_digest.rs");
}
