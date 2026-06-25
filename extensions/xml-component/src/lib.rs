//! XML parse + simple XPath-style extraction as DuckDB scalars (via `roxmltree`):
//!   xml_valid(xml) -> bool,
//!   xml_extract(xml, path) -> text (first match, NULL if none / parse error),
//!   xml_extract_all(xml, path) -> text (JSON array of all matches).
//!
//! Path support is a SIMPLE subset of XPath: absolute element paths only,
//! e.g. `/root/child/leaf`. The leading `/` is optional. Matching is by
//! element name from the document root downward. Wildcards (`*`), descendant
//! axis (`//`), attributes (`@x`), predicates (`[...]`), and functions
//! (`text()`) are NOT supported. The "string value" of a node is the
//! concatenation of all descendant text, matching XPath string-value semantics.
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

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "xml".into(),
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

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Split a simple path like `/a/b/c` (or `a/b/c`) into its element-name steps.
/// Empty / whitespace-only segments are dropped. Returns None if there are no
/// usable steps.
fn path_steps(path: &str) -> Option<std::vec::Vec<&str>> {
    let steps: std::vec::Vec<&str> = path
        .split('/')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if steps.is_empty() {
        None
    } else {
        Some(steps)
    }
}

/// XPath string-value of an element: concatenation of all descendant text.
fn string_value(node: roxmltree::Node) -> std::string::String {
    let mut out = std::string::String::new();
    for d in node.descendants() {
        if d.is_text() {
            if let Some(t) = d.text() {
                out.push_str(t);
            }
        }
    }
    out
}

/// Collect string values of every element matching the absolute path.
fn extract_matches(xml: &str, path: &str) -> Option<std::vec::Vec<std::string::String>> {
    let steps = path_steps(path)?;
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    // First step must match the document root element.
    if root.tag_name().name() != steps[0] {
        return Some(std::vec::Vec::new());
    }
    // BFS/DFS down the remaining steps, tracking the set of nodes at each level.
    let mut current = vec![root];
    for step in &steps[1..] {
        let mut next = std::vec::Vec::new();
        for node in &current {
            for child in node.children().filter(|c| c.is_element()) {
                if child.tag_name().name() == *step {
                    next.push(child);
                }
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    Some(current.iter().map(|n| string_value(*n)).collect())
}

/// Minimal JSON string encoder (escapes the characters JSON requires).
fn json_escape(s: &str) -> std::string::String {
    let mut out = std::string::String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
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
            F::Valid => match text_arg(&args, 0) {
                Some(xml) => types::Duckvalue::Boolean(roxmltree::Document::parse(&xml).is_ok()),
                // NULL or non-text input -> NULL.
                None => types::Duckvalue::Null,
            },
            F::Extract => {
                match (text_arg(&args, 0), text_arg(&args, 1)) {
                    (Some(xml), Some(path)) => match extract_matches(&xml, &path) {
                        Some(matches) if !matches.is_empty() => {
                            types::Duckvalue::Text(matches[0].clone().into())
                        }
                        // No match or parse error -> NULL.
                        _ => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            F::ExtractAll => match (text_arg(&args, 0), text_arg(&args, 1)) {
                (Some(xml), Some(path)) => match extract_matches(&xml, &path) {
                    Some(matches) => {
                        let mut json = std::string::String::from("[");
                        for (i, m) in matches.iter().enumerate() {
                            if i > 0 {
                                json.push(',');
                            }
                            json.push_str(&json_escape(m));
                        }
                        json.push(']');
                        types::Duckvalue::Text(json.into())
                    }
                    // Parse error -> NULL.
                    None => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("xml: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("xml: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("xml: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("xml: no casts".into()))
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

    // xml_valid(xml) -> bool
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, F::Valid);
        reg.register(
            "xml_valid",
            &[runtime::Funcarg {
                name: Some("xml".into()),
                logical: types::Logicaltype::Text,
            }],
            &types::Logicaltype::Boolean,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some("true if the input is well-formed XML".into()),
                tags: vec!["xml".into()],
                attributes: det,
            }),
        )?;
    }

    // xml_extract(xml, path) -> text
    for (name, f, desc) in [
        (
            "xml_extract",
            F::Extract,
            "string value of the first element matching a simple /a/b/c path",
        ),
        (
            "xml_extract_all",
            F::ExtractAll,
            "JSON array of string values of all elements matching a simple /a/b/c path",
        ),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        reg.register(
            name,
            &[
                runtime::Funcarg {
                    name: Some("xml".into()),
                    logical: types::Logicaltype::Text,
                },
                runtime::Funcarg {
                    name: Some("xpath".into()),
                    logical: types::Logicaltype::Text,
                },
            ],
            &types::Logicaltype::Text,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["xml".into()],
                attributes: det,
            }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum F {
    Valid,
    Extract,
    ExtractAll,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
