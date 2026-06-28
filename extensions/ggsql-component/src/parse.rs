//! wit-free VISUALIZE parse/rewrite logic for ggsql (parser-dispatch surface,
//! v3 @3.0.0), shared VERBATIM by the component (`lib.rs`) and the cargo-fuzz
//! target (`fuzz/fuzz_targets/ggsql_parse.rs`).
//!
//! This is a v3 TRUST BOUNDARY: the host hands the component the raw, fully
//! attacker-controlled text of any statement the built-in parser rejected
//! (`parser-dispatch.call-parse`). The contract under test: NEVER PANIC on any
//! input -- garbage, huge, non-UTF-8 (the host hands a String, but the fuzzer
//! drives lossy bytes to explore multi-byte boundaries), deeply nested, or
//! adversarial statements must all come back as `Declined` / `Invalid` / a
//! (possibly nonsensical) `Rewrite` string -- never an abort. The rewrite SQL
//! the host re-plans is the host's concern (the core's binder rejects bad SQL
//! cleanly); this module's job is to not crash while producing it.
//!
//! No wit types appear here, so the file is natively compilable for libfuzzer.

/// Outcome of offering a statement to the ggsql parser, in wit-free terms (the
/// component maps these onto `parser-dispatch.parse-outcome` / `duckerror`).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not a `VISUALIZE` statement; the core proceeds to the next parser
    /// extension / its own parse error.
    Declined,
    /// A malformed `VISUALIZE` (e.g. no inner select); surfaced as an
    /// invalid-argument duckerror carrying this message.
    Invalid(String),
    /// The statement is claimed and rewritten to this ordinary DuckDB SQL.
    Rewrite(String),
}

/// Keyword we intercept, lower-case. Exactly 9 ASCII bytes.
const KW: &str = "visualize";

/// Parse/rewrite a single statement the built-in parser rejected. Pure, total,
/// and panic-free for every `&str`.
pub fn parse_visualize(query: &str) -> Outcome {
    let trimmed = query.trim().trim_end_matches(';').trim();

    // Case-insensitive `VISUALIZE` prefix check over the FIRST `KW.len()` chars.
    // `eq_ignore_ascii_case` compares bytes ignoring ASCII case and is false when
    // the byte lengths differ, so a head holding any multi-byte char (whose byte
    // length != 9) can never match. Therefore a match guarantees the head is
    // exactly 9 ASCII bytes => byte index `KW.len()` is a valid char boundary and
    // the slice below cannot panic. (Replaces the old allocate-and-compare form.)
    let head: String = trimmed.chars().take(KW.len()).collect();
    if !head.eq_ignore_ascii_case(KW) {
        return Outcome::Declined;
    }

    let inner = trimmed[KW.len()..].trim();
    if inner.is_empty() {
        return Outcome::Invalid(
            "VISUALIZE requires a SELECT statement, e.g. VISUALIZE SELECT region, n FROM t".into(),
        );
    }

    // Rewrite: wrap the inner select as a CTE and emit a (label, n, bar) rollup.
    // The inner select is expected to project (label, value); we render a unit
    // bar of '#' repeated by value. This desugars entirely to standard SQL --
    // the whole point of the string->SQL rewrite form. A nonsensical `inner`
    // yields nonsensical-but-syntactically-embedded SQL; the core's binder is
    // what rejects it (cleanly), not us.
    let rewritten = format!(
        "WITH __viz AS ({inner}) \
         SELECT CAST(label AS VARCHAR) AS label, \
                CAST(n AS BIGINT) AS n, \
                repeat('#', GREATEST(CAST(n AS BIGINT), 0)) AS bar \
         FROM (SELECT * FROM __viz) AS t(label, n) \
         ORDER BY n DESC"
    );
    Outcome::Rewrite(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declines_non_visualize() {
        assert_eq!(parse_visualize("SELECT 1"), Outcome::Declined);
        assert_eq!(parse_visualize(""), Outcome::Declined);
        assert_eq!(parse_visualize("   "), Outcome::Declined);
        assert_eq!(parse_visualize("vis"), Outcome::Declined);
    }

    #[test]
    fn rewrites_visualize_case_insensitive() {
        for q in ["VISUALIZE SELECT a, b FROM t", "visualize select a,b from t;", "  ViSuAlIzE SELECT 1, 2 ; "] {
            match parse_visualize(q) {
                Outcome::Rewrite(sql) => assert!(sql.contains("__viz")),
                other => panic!("expected rewrite, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_inner_is_invalid() {
        assert!(matches!(parse_visualize("VISUALIZE"), Outcome::Invalid(_)));
        assert!(matches!(parse_visualize("VISUALIZE ;"), Outcome::Invalid(_)));
        assert!(matches!(parse_visualize("  visualize   "), Outcome::Invalid(_)));
    }

    /// Regression: a multi-byte char in the keyword window must not panic on the
    /// byte slice `trimmed[KW.len()..]` (the head check guarantees the slice is on
    /// a char boundary only when it matches; here it must DECLINE, not slice).
    #[test]
    fn multibyte_prefix_does_not_panic() {
        // 'é' etc. in the first 9 chars => head has >9 bytes => declines.
        assert_eq!(parse_visualize("visualizé SELECT 1"), Outcome::Declined);
        assert_eq!(parse_visualize("v\u{0131}sualize SELECT 1"), Outcome::Declined);
        // A snowman immediately after a partial keyword.
        let _ = parse_visualize("visual\u{2603}ze SELECT 1");
        // Pure multi-byte garbage.
        let _ = parse_visualize("\u{1F4A9}\u{1F4A9}\u{1F4A9}\u{1F4A9}\u{1F4A9}");
    }
}
