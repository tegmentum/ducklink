//! Fuzz the bencode component's decode-to-JSON path (untrusted BLOB bytes).
//!
//! `bencode-component/src/lib.rs` runs `wit_bindgen::generate!`, so its pure
//! decode functions (`decode_to_json` + the recursive `ben_to_json` /
//! `bytes_to_json_string` / `escape_json_string`) are mirrored VERBATIM here.
//! They depend only on `serde_bencode` and `std`. The component's own
//! `#[cfg(test)]` module holds the regression tests.
//!
//! Contract: any `&[u8]` -> `Some(json)` or `None`, never a panic. Deeply nested
//! bencode lists/dicts (recursion), huge length prefixes, and non-UTF-8 byte
//! strings must all be handled without an abort.
#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_bencode::value::Value as Ben;

fn bytes_to_json_string(bytes: &[u8], out: &mut String) {
    match std::str::from_utf8(bytes) {
        Ok(s) => escape_json_string(s, out),
        Err(_) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                hex.push_str(&format!("{:02x}", b));
            }
            escape_json_string(&hex, out);
        }
    }
}

fn escape_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn ben_to_json(v: &Ben, out: &mut String) {
    match v {
        Ben::Int(i) => out.push_str(&i.to_string()),
        Ben::Bytes(b) => bytes_to_json_string(b, out),
        Ben::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                ben_to_json(item, out);
            }
            out.push(']');
        }
        Ben::Dict(map) => {
            let mut entries: Vec<(&Vec<u8>, &Ben)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (k, val)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                bytes_to_json_string(k, out);
                out.push(':');
                ben_to_json(val, out);
            }
            out.push('}');
        }
    }
}

fn decode_to_json(data: &[u8]) -> Option<String> {
    let v: Ben = serde_bencode::from_bytes(data).ok()?;
    let mut out = String::new();
    ben_to_json(&v, &mut out);
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    let _ = decode_to_json(data);
});
