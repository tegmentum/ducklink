//! Fuzz the ggsql parser-extension parse/rewrite logic (v3 @3.0.0
//! `parser-dispatch.call-parse` trust boundary).
//!
//! The host hands a parser component the raw text of ANY statement the built-in
//! parser rejected. `parse.rs` is wit-free (std only), so we `#[path]`-include it
//! and drive `parse_visualize` from the libfuzzer byte buffer: garbage SQL, huge
//! inputs, non-UTF-8 (driven via lossy conversion to explore multi-byte
//! boundaries around the `VISUALIZE` keyword byte-slice), deeply nested
//! parentheses, and statements that nearly-but-don't match the keyword.
//!
//! Contract under test: ANY input -> `Declined` / `Invalid` / `Rewrite`, never a
//! panic. (The rewrite SQL itself is re-planned by the core's binder, which
//! rejects bad SQL cleanly; this proves the component never crashes producing it.)
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../extensions/ggsql-component/src/parse.rs"]
mod parse;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // The real callsite receives a DuckDB VARCHAR (valid UTF-8); lossy conversion
    // keeps the fuzzer exploring multi-byte boundaries that the keyword-slice sees.
    let s = String::from_utf8_lossy(data);
    let _ = parse::parse_visualize(&s);

    // Also force the keyword path: prepend a (case-mutated) `VISUALIZE ` so the
    // corpus reliably reaches the inner-select extraction + rewrite, not just the
    // decline fast-path. The first byte selects the keyword casing.
    let kw = match data[0] % 4 {
        0 => "VISUALIZE ",
        1 => "visualize ",
        2 => "ViSuAlIzE ",
        _ => "VISUALIZE;",
    };
    let mut forced = String::with_capacity(kw.len() + s.len());
    forced.push_str(kw);
    forced.push_str(&s);
    let _ = parse::parse_visualize(&forced);
});
