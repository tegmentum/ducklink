//! mailto: URI parser (RFC 6068) as DuckDB scalars:
//!   mailto_to(uri)            -> JSON array of primary recipient addresses,
//!   mailto_field(uri, name)   -> percent-decoded header value (NULL if absent),
//!   mailto_to_json(uri)       -> {to:[...], subject, body, cc, bcc} JSON
//!                                (absent fields omitted).
//! Non-mailto: / malformed / NULL input -> NULL. Never panics.
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

use percent_encoding::percent_decode_str;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "mailto".into(),
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

// --- RFC 6068 mailto: parsing -------------------------------------------------

/// Percent-decode a component. Note: per RFC 6068, '+' is a literal plus, NOT a
/// space (mailto: is not form-urlencoded). We therefore decode only %XX.
fn pct_decode(s: &str) -> Option<std::string::String> {
    percent_decode_str(s).decode_utf8().ok().map(|c| c.into_owned())
}

/// A parsed mailto: URI. Addresses and header values are percent-decoded.
struct Mailto {
    to: std::vec::Vec<std::string::String>,
    // header name (lowercased) -> first decoded value
    headers: std::collections::BTreeMap<std::string::String, std::string::String>,
}

fn parse_mailto(uri: &str) -> Option<Mailto> {
    // Scheme is case-insensitive.
    let rest = uri
        .strip_prefix("mailto:")
        .or_else(|| uri.strip_prefix("MAILTO:"))
        .or_else(|| {
            let lower = uri.get(..7)?.to_ascii_lowercase();
            if lower == "mailto:" {
                uri.get(7..)
            } else {
                None
            }
        })?;

    // Split path (to-list) from the query (hfields).
    let (to_part, query) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };

    let mut to: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    if !to_part.is_empty() {
        for addr in to_part.split(',') {
            if addr.is_empty() {
                continue;
            }
            let d = pct_decode(addr)?;
            if d.is_empty() {
                return None;
            }
            to.push(d);
        }
    }

    let mut headers: std::collections::BTreeMap<std::string::String, std::string::String> =
        std::collections::BTreeMap::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.find('=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => return None, // malformed hfield
            };
            let name_dec = pct_decode(name)?;
            let value_dec = pct_decode(value)?;
            let key = name_dec.to_ascii_lowercase();
            // A "to" header augments the recipient list per RFC 6068.
            if key == "to" {
                for addr in value_dec.split(',') {
                    if !addr.is_empty() {
                        to.push(addr.to_string());
                    }
                }
            }
            // First occurrence wins for header lookups.
            headers.entry(key).or_insert(value_dec);
        }
    }

    // A mailto: with no recipients anywhere AND no headers is degenerate but
    // still valid ("mailto:"). We accept it (empty to-list).
    Some(Mailto { to, headers })
}

// --- Minimal JSON encoding ----------------------------------------------------

fn json_escape(s: &str, out: &mut std::string::String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_array(items: &[std::string::String]) -> std::string::String {
    let mut out = std::string::String::new();
    out.push('[');
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_escape(it, &mut out);
    }
    out.push(']');
    out
}

fn mailto_to_json_str(m: &Mailto) -> std::string::String {
    let mut out = std::string::String::new();
    out.push('{');
    let mut first = true;
    let comma = |out: &mut std::string::String, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            out.push(',');
        }
    };

    if !m.to.is_empty() {
        comma(&mut out, &mut first);
        out.push_str("\"to\":");
        out.push_str(&json_array(&m.to));
    }
    // Emit known fields in a stable order; omit absent ones.
    for key in ["subject", "body", "cc", "bcc"] {
        if let Some(v) = m.headers.get(key) {
            comma(&mut out, &mut first);
            json_escape(key, &mut out);
            out.push(':');
            json_escape(v, &mut out);
        }
    }
    out.push('}');
    out
}

// --- dispatch -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum F {
    To,
    Field,
    ToJson,
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

        let uri = match text_arg(&args, 0) {
            Some(u) => u,
            None => return Ok(types::Duckvalue::Null),
        };

        Ok(match which {
            F::To => match parse_mailto(&uri) {
                Some(m) => types::Duckvalue::Text(json_array(&m.to).into()),
                None => types::Duckvalue::Null,
            },
            F::Field => {
                let name = match text_arg(&args, 1) {
                    Some(n) => n.to_ascii_lowercase(),
                    None => return Ok(types::Duckvalue::Null),
                };
                match parse_mailto(&uri) {
                    Some(m) => match m.headers.get(name.as_str()) {
                        Some(v) => types::Duckvalue::Text(v.clone().into()),
                        None => types::Duckvalue::Null,
                    },
                    None => types::Duckvalue::Null,
                }
            }
            F::ToJson => match parse_mailto(&uri) {
                Some(m) => types::Duckvalue::Text(mailto_to_json_str(&m).into()),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mailto: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mailto: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mailto: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mailto: no casts".into()))
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
    let tags = || vec!["networking".into(), "email".into()];

    // mailto_to(uri) -> text
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, F::To);
    reg.register(
        "mailto_to",
        &[runtime::Funcarg {
            name: Some("uri".into()),
            logical: types::Logicaltype::Text,
        }],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("mailto: URI -> JSON array of recipient addresses".into()),
            tags: tags(),
            attributes: det,
        }),
    )?;

    // mailto_field(uri, name) -> text
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, F::Field);
    reg.register(
        "mailto_field",
        &[
            runtime::Funcarg {
                name: Some("uri".into()),
                logical: types::Logicaltype::Text,
            },
            runtime::Funcarg {
                name: Some("name".into()),
                logical: types::Logicaltype::Text,
            },
        ],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("mailto: header value (percent-decoded), NULL if absent".into()),
            tags: tags(),
            attributes: det,
        }),
    )?;

    // mailto_to_json(uri) -> text
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, F::ToJson);
    reg.register(
        "mailto_to_json",
        &[runtime::Funcarg {
            name: Some("uri".into()),
            logical: types::Logicaltype::Text,
        }],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("mailto: URI -> {to,subject,body,cc,bcc} JSON".into()),
            tags: tags(),
            attributes: det,
        }),
    )?;

    Ok(())
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
