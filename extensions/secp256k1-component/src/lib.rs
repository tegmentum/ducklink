//! ECDSA on the secp256k1 curve as DuckDB scalars (via `k256`). All I/O is BLOB:
//!   secp256k1_pubkey(privkey BLOB) -> BLOB   -- 32-byte priv -> 33-byte compressed pub
//!   secp256k1_sign(msg_hash BLOB, privkey BLOB) -> BLOB  -- 32-byte hash -> 64-byte compact sig
//!   secp256k1_verify(msg_hash BLOB, signature BLOB, pubkey BLOB) -> BOOLEAN
//! Signing is deterministic (RFC 6979, k256 default). Bad input -> NULL/false, never panics.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use k256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "secp256k1".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

fn blob_arg(args: &[types::Duckvalue], i: usize) -> Option<std::vec::Vec<u8>> {
    match args.get(i) {
        Some(types::Duckvalue::Blob(b)) => Some(b.clone().into()),
        _ => None,
    }
}

/// 32-byte privkey -> 33-byte compressed pubkey, or None.
fn pubkey(priv_bytes: &[u8]) -> Option<std::vec::Vec<u8>> {
    if priv_bytes.len() != 32 {
        return None;
    }
    let sk = SigningKey::from_bytes(priv_bytes.into()).ok()?;
    let vk = VerifyingKey::from(&sk);
    Some(vk.to_encoded_point(true).as_bytes().to_vec())
}

/// Deterministic (RFC 6979) sign of a 32-byte prehash -> 64-byte compact sig, or None.
fn sign(hash: &[u8], priv_bytes: &[u8]) -> Option<std::vec::Vec<u8>> {
    if hash.len() != 32 || priv_bytes.len() != 32 {
        return None;
    }
    let sk = SigningKey::from_bytes(priv_bytes.into()).ok()?;
    let sig: Signature = sk.sign_prehash(hash).ok()?;
    // Normalize to low-S so the signature is canonical/deterministic.
    let sig = sig.normalize_s().unwrap_or(sig);
    Some(sig.to_bytes().to_vec())
}

/// Verify a 64-byte compact sig over a 32-byte prehash with a 33/65-byte pubkey.
/// None on malformed input; Some(bool) otherwise.
fn verify(hash: &[u8], sig_bytes: &[u8], pub_bytes: &[u8]) -> Option<bool> {
    if hash.len() != 32 || sig_bytes.len() != 64 {
        return None;
    }
    let vk = VerifyingKey::from_sec1_bytes(pub_bytes).ok()?;
    let sig = Signature::from_slice(sig_bytes).ok()?;
    // Accept either normalization on input.
    let sig = sig.normalize_s().unwrap_or(sig);
    Some(vk.verify_prehash(hash, &sig).is_ok())
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        h: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(
                h,
                a,
                types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow },
            )?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            S::Pubkey => match blob_arg(&args, 0).and_then(|pk| pubkey(&pk)) {
                Some(b) => types::Duckvalue::Blob(b.into()),
                None => types::Duckvalue::Null,
            },
            S::Sign => match (blob_arg(&args, 0), blob_arg(&args, 1)) {
                (Some(hash), Some(pk)) => match sign(&hash, &pk) {
                    Some(b) => types::Duckvalue::Blob(b.into()),
                    None => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
            S::Verify => match (blob_arg(&args, 0), blob_arg(&args, 1), blob_arg(&args, 2)) {
                (Some(hash), Some(sig), Some(pk)) => match verify(&hash, &sig, &pk) {
                    Some(ok) => types::Duckvalue::Boolean(ok),
                    None => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("secp256k1: no table fns".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("secp256k1: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("secp256k1: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("secp256k1: no casts".into()))
    }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let tag = || vec!["crypto".into()];

    // secp256k1_pubkey(privkey BLOB) -> BLOB
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, S::Pubkey);
    reg.register(
        "secp256k1_pubkey",
        &[runtime::Funcarg { name: Some("privkey".into()), logical: types::Logicaltype::Blob }],
        types::Logicaltype::Blob,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("32-byte private key -> 33-byte compressed public key".into()),
            tags: tag(),
            attributes: det,
        }),
    )?;

    // secp256k1_sign(msg_hash BLOB, privkey BLOB) -> BLOB
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, S::Sign);
    reg.register(
        "secp256k1_sign",
        &[
            runtime::Funcarg { name: Some("msg_hash".into()), logical: types::Logicaltype::Blob },
            runtime::Funcarg { name: Some("privkey".into()), logical: types::Logicaltype::Blob },
        ],
        types::Logicaltype::Blob,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("ECDSA sign a 32-byte hash (RFC 6979) -> 64-byte compact sig".into()),
            tags: tag(),
            attributes: det,
        }),
    )?;

    // secp256k1_verify(msg_hash BLOB, signature BLOB, pubkey BLOB) -> BOOLEAN
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, S::Verify);
    reg.register(
        "secp256k1_verify",
        &[
            runtime::Funcarg { name: Some("msg_hash".into()), logical: types::Logicaltype::Blob },
            runtime::Funcarg { name: Some("signature".into()), logical: types::Logicaltype::Blob },
            runtime::Funcarg { name: Some("pubkey".into()), logical: types::Logicaltype::Blob },
        ],
        types::Logicaltype::Boolean,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("Verify a 64-byte compact ECDSA sig over a 32-byte hash".into()),
            tags: tag(),
            attributes: det,
        }),
    )?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum S {
    Pubkey,
    Sign,
    Verify,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, S>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, S>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
