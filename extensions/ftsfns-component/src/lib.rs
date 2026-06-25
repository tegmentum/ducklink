//! Full-text-search FUNCTION layer as DuckDB scalars (pure Rust: `rust-stemmers`
//! Snowball/Porter + a simple tokenizer). Reimplements the feasible scalar part
//! of DuckDB's `fts` extension so fts can be partially de-embedded from the lean
//! core. Functions:
//!   fts_tokenize(text) -> text          JSON array of lowercased word tokens
//!   fts_stem(word, language) -> text    Porter/Snowball stem (default english)
//!   fts_stem_text(text) -> text         tokenize + stem each -> JSON array
//!   bm25_score(tf, df, doc_len, avg_doc_len, num_docs) -> double  Okapi BM25
//!   fts_match(doc, query) -> boolean    all stemmed query tokens in stemmed doc
//! NULL / invalid -> NULL (never panics). The fts_ prefix avoids colliding with
//! the existing `stem` component on the lean core.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

mod core {
    //! Pure-Rust core logic, free of WIT types, so it can be unit tested natively.
    use rust_stemmers::{Algorithm, Stemmer};

    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    /// Split on non-alphanumeric, lowercase, drop empties.
    pub fn tokenize(text: &str) -> Vec<std::string::String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .collect()
    }

    /// JSON array of lowercased word tokens.
    pub fn tokenize_json(text: &str) -> std::string::String {
        serde_json::to_string(&tokenize(text)).unwrap_or_else(|_| "[]".into())
    }

    /// Map a language name/code to a Snowball algorithm; None if unknown.
    pub fn algorithm(lang: &str) -> Option<Algorithm> {
        match lang.trim().to_ascii_lowercase().as_str() {
            "english" | "en" | "porter" => Some(Algorithm::English),
            "french" | "fr" => Some(Algorithm::French),
            "german" | "de" => Some(Algorithm::German),
            "spanish" | "es" => Some(Algorithm::Spanish),
            "italian" | "it" => Some(Algorithm::Italian),
            "portuguese" | "pt" => Some(Algorithm::Portuguese),
            "russian" | "ru" => Some(Algorithm::Russian),
            "dutch" | "nl" => Some(Algorithm::Dutch),
            "swedish" | "sv" => Some(Algorithm::Swedish),
            "norwegian" | "no" => Some(Algorithm::Norwegian),
            "danish" | "da" => Some(Algorithm::Danish),
            "finnish" | "fi" => Some(Algorithm::Finnish),
            _ => None,
        }
    }

    /// Stem one word; None for unknown language.
    pub fn stem(word: &str, lang: &str) -> Option<std::string::String> {
        let alg = algorithm(lang)?;
        let stemmer = Stemmer::create(alg);
        Some(stemmer.stem(&word.to_lowercase()).into_owned())
    }

    /// Tokenize then English-stem each token; JSON array.
    pub fn stem_text_json(text: &str) -> std::string::String {
        let stemmer = Stemmer::create(Algorithm::English);
        let stemmed: Vec<std::string::String> = tokenize(text)
            .iter()
            .map(|t| stemmer.stem(t).into_owned())
            .collect();
        serde_json::to_string(&stemmed).unwrap_or_else(|_| "[]".into())
    }

    /// Okapi BM25 single-term score (k1=1.2, b=0.75).
    pub fn bm25_score(tf: i64, df: i64, doc_len: f64, avg_doc_len: f64, num_docs: i64) -> f64 {
        let tf = tf as f64;
        let df = df as f64;
        let n = num_docs as f64;
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        let avg = if avg_doc_len == 0.0 { 1.0 } else { avg_doc_len };
        let denom = tf + K1 * (1.0 - B + B * doc_len / avg);
        idf * (tf * (K1 + 1.0)) / denom
    }

    /// Quote a SQL identifier by doubling embedded double-quotes and wrapping in
    /// double-quotes. Inputs come from PRAGMA args, so this must be robust.
    pub fn quote_ident(name: &str) -> std::string::String {
        let mut out = std::string::String::with_capacity(name.len() + 2);
        out.push('"');
        for c in name.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
        out
    }

    /// Generate the SQL script that `PRAGMA create_fts_index(table, id, textcol)`
    /// returns; the core runs it on the connection. Builds an inverted index over
    /// the English-stemmed tokens of `textcol`, plus a `match_bm25(docid, query)`
    /// macro that sums Okapi BM25 over the stemmed query terms. Object names are
    /// derived from the table; identifiers are quoted. A simplified-but-correct
    /// BM25 over stemmed terms (k1=1.2, b=0.75 in bm25_score).
    pub fn build_fts_index_sql(table: &str, id: &str, textcol: &str) -> std::string::String {
        let t = quote_ident(table);
        let idc = quote_ident(id);
        let txt = quote_ident(textcol);
        // Index object names: fts_<table>_<suffix>. Sanitize the table for the
        // object-name part (keep it simple/identifier-safe); also quote it.
        let safe: std::string::String = table
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        let terms = quote_ident(&format!("fts_{safe}_terms"));
        let docs = quote_ident(&format!("fts_{safe}_docs"));
        let dict = quote_ident(&format!("fts_{safe}_dict"));
        let stats = quote_ident(&format!("fts_{safe}_stats"));
        let macro_name = quote_ident(&format!("match_bm25"));
        format!(
            "DROP TABLE IF EXISTS {terms};\n\
             CREATE TABLE {terms} AS \
               SELECT src.{idc} AS docid, term \
               FROM {t} AS src, UNNEST(CAST(fts_stem_text(src.{txt}) AS VARCHAR[])) AS exploded(term);\n\
             DROP TABLE IF EXISTS {docs};\n\
             CREATE TABLE {docs} AS \
               SELECT docid, CAST(COUNT(*) AS BIGINT) AS doc_len FROM {terms} GROUP BY docid;\n\
             DROP TABLE IF EXISTS {dict};\n\
             CREATE TABLE {dict} AS \
               SELECT term, CAST(COUNT(DISTINCT docid) AS BIGINT) AS df FROM {terms} GROUP BY term;\n\
             DROP TABLE IF EXISTS {stats};\n\
             CREATE TABLE {stats} AS \
               SELECT CAST(COUNT(*) AS BIGINT) AS num_docs, \
                      CAST(COALESCE(AVG(doc_len), 0.0) AS DOUBLE) AS avg_doc_len FROM {docs};\n\
             CREATE OR REPLACE MACRO {macro_name}(p_docid, p_query) AS (\
               SELECT COALESCE(SUM(bm25_score(tf.tf, dict.df, d.doc_len::DOUBLE, s.avg_doc_len, s.num_docs)), 0.0) \
               FROM (SELECT term, CAST(COUNT(*) AS BIGINT) AS tf FROM {terms} \
                     WHERE docid = p_docid GROUP BY term) AS tf \
               JOIN (SELECT DISTINCT term FROM \
                       UNNEST(CAST(fts_stem_text(p_query) AS VARCHAR[])) AS q(term)) AS qt \
                 ON qt.term = tf.term \
               JOIN {dict} AS dict ON dict.term = tf.term \
               JOIN {docs} AS d ON d.docid = p_docid \
               CROSS JOIN {stats} AS s\
             );\n"
        )
    }

    /// True if every English-stemmed query token appears among the doc's
    /// English-stemmed tokens (simple AND match).
    pub fn fts_match(doc: &str, query: &str) -> bool {
        let stemmer = Stemmer::create(Algorithm::English);
        let doc_tokens: std::collections::HashSet<std::string::String> =
            tokenize(doc).iter().map(|t| stemmer.stem(t).into_owned()).collect();
        let q: Vec<std::string::String> =
            tokenize(query).iter().map(|t| stemmer.stem(t).into_owned()).collect();
        if q.is_empty() {
            return false;
        }
        q.iter().all(|t| doc_tokens.contains(t))
    }
}

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        register_pragmas()?;
        Ok(types::Loadresult { name: "ftsfns".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        Some(types::Duckvalue::Uint64(v)) => Some(*v as i64),
        Some(types::Duckvalue::Float64(v)) => Some(*v as i64),
        _ => None,
    }
}
fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        Some(types::Duckvalue::Uint64(v)) => Some(*v as f64),
        _ => None,
    }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            F::Tokenize => match text_arg(&args, 0) {
                Some(s) => types::Duckvalue::Text(core::tokenize_json(&s).into()),
                None => types::Duckvalue::Null,
            },
            F::Stem => {
                let word = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
                let lang = text_arg(&args, 1).unwrap_or_else(|| "english".into());
                match core::stem(&word, &lang) {
                    Some(s) => types::Duckvalue::Text(s.into()),
                    None => types::Duckvalue::Null,
                }
            }
            F::StemText => match text_arg(&args, 0) {
                Some(s) => types::Duckvalue::Text(core::stem_text_json(&s).into()),
                None => types::Duckvalue::Null,
            },
            F::Bm25 => {
                let tf = i64_arg(&args, 0); let df = i64_arg(&args, 1);
                let doc_len = f64_arg(&args, 2); let avg = f64_arg(&args, 3);
                let n = i64_arg(&args, 4);
                match (tf, df, doc_len, avg, n) {
                    (Some(tf), Some(df), Some(dl), Some(adl), Some(n)) =>
                        types::Duckvalue::Float64(core::bm25_score(tf, df, dl, adl, n)),
                    _ => types::Duckvalue::Null,
                }
            }
            F::Match => {
                let doc = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
                let q = match text_arg(&args, 1) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
                types::Duckvalue::Boolean(core::fts_match(&doc, &q))
            }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ftsfns: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ftsfns: no aggs".into())) }
    // Item 4: `PRAGMA create_fts_index('<table>','<id>','<textcol>')`. The core
    // intercepts the PRAGMA, dispatches here mid-query, and we RETURN a SQL script
    // (we do NOT re-enter SQL ourselves) that the core then runs on the connection:
    // it builds an inverted-index (terms/docs/dict/stats tables) by tokenizing +
    // English-stemming the text column (reusing our fts_stem_text scalar), and a
    // per-index `match_bm25(docid, query)` macro summing bm25_score over the
    // stemmed query terms. No connection re-entrancy.
    fn call_pragma(handle: u32, args: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        let which = pragma_handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown pragma handle".into()))?;
        match which {
            P::CreateFtsIndex => {
                let table = text_arg(&args, 0)
                    .ok_or_else(|| types::Duckerror::Invalidargument("create_fts_index: table name (arg 0) required".into()))?;
                let id = text_arg(&args, 1)
                    .ok_or_else(|| types::Duckerror::Invalidargument("create_fts_index: id column (arg 1) required".into()))?;
                let textcol = text_arg(&args, 2)
                    .ok_or_else(|| types::Duckerror::Invalidargument("create_fts_index: text column (arg 2) required".into()))?;
                let script = core::build_fts_index_sql(&table, &id, &textcol);
                Ok(Some(types::Duckvalue::Text(script.into())))
            }
        }
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ftsfns: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let txt = |name: &str| runtime::Funcarg { name: Some(name.into()), logical: types::Logicaltype::Text };

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Tokenize);
    reg.register("fts_tokenize", &[txt("text")], types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("tokenize text -> JSON array of lowercased words".into()), tags: vec!["fts".into(), "nlp".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Stem);
    reg.register("fts_stem", &[txt("word"), txt("language")], types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("Snowball/Porter stem (default english); unknown language -> NULL".into()), tags: vec!["fts".into(), "nlp".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::StemText);
    reg.register("fts_stem_text", &[txt("text")], types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("tokenize + English-stem each -> JSON array".into()), tags: vec!["fts".into(), "nlp".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Bm25);
    reg.register("bm25_score", &[
        runtime::Funcarg { name: Some("tf".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("df".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("doc_len".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("avg_doc_len".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("num_docs".into()), logical: types::Logicaltype::Int64 }],
        types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("Okapi BM25 term score (k1=1.2, b=0.75)".into()), tags: vec!["fts".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Match);
    reg.register("fts_match", &[txt("doc"), txt("query")], types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("true if all stemmed query tokens appear in stemmed doc (AND match)".into()), tags: vec!["fts".into()], attributes: det }))?;
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum F { Tokenize, Stem, StemText, Bm25, Match }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }

// Item 4: pragma capability. `create_fts_index` is the only pragma; it returns a
// SQL script (see core::build_fts_index_sql) that the core runs on the connection.
#[derive(Clone, Copy, PartialEq)] enum P { CreateFtsIndex }
static PRAGMA_HANDLERS: OnceLock<Mutex<HashMap<u32, P>>> = OnceLock::new();
fn pragma_handlers() -> &'static Mutex<HashMap<u32, P>> { PRAGMA_HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }

fn register_pragmas() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Pragma)
        .ok_or_else(|| types::Duckerror::Internal("no pragma capability".into()))?;
    let reg = match cap { runtime::Capability::Pragma(r) => r, _ => return Err(types::Duckerror::Internal("bad pragma capability".into())) };
    let txt = |name: &str| runtime::Funcarg { name: Some(name.into()), logical: types::Logicaltype::Text };

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    pragma_handlers().lock().unwrap().insert(h, P::CreateFtsIndex);
    reg.register_call(
        "create_fts_index",
        &[txt("table_name"), txt("id_column"), txt("text_column")],
        types::Logicaltype::Text,
        runtime::PragmaCallback::new(h),
        Some(&runtime::Extopts {
            description: Some("build an inverted FTS index + match_bm25 macro over a text column".into()),
            tags: vec!["fts".into()],
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::core;
    #[test]
    fn stem_running() {
        assert_eq!(core::stem("running", "english").as_deref(), Some("run"));
    }
    #[test]
    fn stem_unknown_language_is_none() {
        assert!(core::stem("running", "klingon").is_none());
    }
    #[test]
    fn tokenize_splits_non_alnum() {
        assert_eq!(core::tokenize_json("The Quick, brown-fox!"), r#"["the","quick","brown","fox"]"#);
    }
    #[test]
    fn stem_text_json_works() {
        assert_eq!(core::stem_text_json("running foxes"), r#"["run","fox"]"#);
    }
    #[test]
    fn bm25_known_value() {
        // tf=1, df=1, doc_len=avg_doc_len -> denom = 1 + 1.2 = 2.2.
        // idf = ln((1-1+0.5)/(1+0.5) + 1) = ln(0.5/1.5 + 1) = ln(1.3333..).
        // score = idf * (1*2.2)/2.2 = idf.
        let s = core::bm25_score(1, 1, 10.0, 10.0, 1);
        let expected = (0.5_f64 / 1.5 + 1.0).ln();
        assert!((s - expected).abs() < 1e-9, "got {s}, want {expected}");
    }
    #[test]
    fn bm25_larger_corpus() {
        // Sanity: more docs with same df raises idf, score stays positive.
        let s = core::bm25_score(3, 2, 12.0, 10.0, 100);
        assert!(s > 0.0);
    }
    #[test]
    fn match_stemmed_singular_in_plural_doc() {
        assert!(core::fts_match("the quick brown foxes", "fox"));
    }
    #[test]
    fn match_and_semantics() {
        assert!(core::fts_match("the quick brown foxes jump", "fox jumping"));
        assert!(!core::fts_match("the quick brown foxes", "cat"));
        assert!(!core::fts_match("anything", ""));
    }
}
