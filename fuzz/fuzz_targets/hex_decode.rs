//! Fuzz the SQLite component's `hex_decode` (the untrusted hex-string -> bytes
//! decoder used to ingest a SQLite DB passed as a hex VARCHAR).
//!
//! `hex_decode` lives in sqlitewasm-component/src/lib.rs, but that file runs the
//! `wit_bindgen::generate!` macro and so cannot be `#[path]`-included natively.
//! The function is tiny and self-contained (no wit types), so it is mirrored
//! VERBATIM here. The component's own `#[cfg(test)]` module holds the regression
//! tests; this target proves the algorithm never panics on adversarial UTF-8.
//!
//! Contract: any input string -> `Some(bytes)` or `None`, never a panic
//! (odd length, non-hex chars, embedded NULs, huge inputs).
#![no_main]

use libfuzzer_sys::fuzz_target;

/// MIRROR of `sqlitewasm-component/src/lib.rs::hex_decode`. Keep in sync.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        out.push((nib(b[i])? << 4) | nib(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    // Treat the bytes as a (possibly invalid) UTF-8 string; the real callsite
    // receives a DuckDB VARCHAR, which is valid UTF-8, but lossy conversion keeps
    // the fuzzer exploring multi-byte boundaries that `.trim()` / `s.len()` see.
    let s = String::from_utf8_lossy(data);
    let _ = hex_decode(&s);
});
