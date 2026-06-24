//! HOCON (Typesafe Config) parsing as DuckDB scalars, via the `hocon` crate.
//!   hocon_to_json(text) -> JSON object string (substitutions resolved by the
//!     crate during parse). Invalid input -> NULL.
//!   hocon_get(text, path) -> value at a dotted path (e.g. 'db.host') as text;
//!     NULL if the path is absent or the input is invalid.
//! Never panics; all error paths return NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use hocon::{Hocon, HoconLoader};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "hocon".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn arg_text(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Parse a HOCON document. URL-include support is compiled out (the crate's
/// `url-support` feature is disabled), so no network is reachable from wasm.
/// The system-environment fallback stays enabled so ${?ENV} resolves cleanly.
fn parse(text: &str) -> Option<Hocon> {
    HoconLoader::new()
        .load_str(text)
        .ok()?
        .hocon()
        .ok()
}

/// Convert a parsed `Hocon` tree into a `serde_json::Value`. `BadValue` nodes
/// (parse/lookup errors) map to JSON null.
fn hocon_to_value(h: &Hocon) -> serde_json::Value {
    use serde_json::Value;
    match h {
        Hocon::Real(r) => serde_json::Number::from_f64(*r)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Hocon::Integer(i) => Value::Number((*i).into()),
        Hocon::String(s) => Value::String(s.clone()),
        Hocon::Boolean(b) => Value::Bool(*b),
        Hocon::Array(a) => Value::Array(a.iter().map(hocon_to_value).collect()),
        Hocon::Hash(m) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in m.iter() {
                obj.insert(k.clone(), hocon_to_value(v));
            }
            Value::Object(obj)
        }
        Hocon::Null | Hocon::BadValue(_) => Value::Null,
    }
}

/// Walk a dotted path (e.g. "db.host"). Numeric segments index arrays.
/// Returns the leaf as a text string, or None if absent.
fn get_path(text: &str, path: &str) -> Option<std::string::String> {
    let root = parse(text)?;
    let mut cur = &root;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        let next = match cur {
            Hocon::Array(_) => match seg.parse::<usize>() {
                Ok(i) => &cur[i],
                Err(_) => return None,
            },
            Hocon::Hash(_) => &cur[seg],
            _ => return None,
        };
        if let Hocon::BadValue(_) = next {
            return None;
        }
        cur = next;
    }
    match cur {
        Hocon::String(s) => Some(s.clone()),
        Hocon::Integer(i) => Some(i.to_string()),
        Hocon::Real(r) => Some(r.to_string()),
        Hocon::Boolean(b) => Some(b.to_string()),
        Hocon::Null => None,
        // Compound values (hash/array) are rendered as JSON text.
        Hocon::Array(_) | Hocon::Hash(_) => {
            serde_json::to_string(&hocon_to_value(cur)).ok()
        }
        Hocon::BadValue(_) => None,
    }
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
        let r: Option<std::string::String> = if handle == 1 {
            // hocon_to_json(text)
            match arg_text(&args, 0) {
                Some(s) => parse(&s).map(|h| hocon_to_value(&h)).and_then(|v| serde_json::to_string(&v).ok()),
                None => None,
            }
        } else {
            // hocon_get(text, path)
            match (arg_text(&args, 0), arg_text(&args, 1)) {
                (Some(s), Some(p)) => get_path(&s, &p),
                _ => None,
            }
        };
        Ok(match r {
            Some(t) => types::Duckvalue::Text(t.into()),
            None => types::Duckvalue::Null,
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hocon: no table fns".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hocon: no aggs".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hocon: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("hocon: no casts".into()))
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
    reg.register(
        "hocon_to_json",
        &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts {
            description: Some("HOCON -> JSON object".into()),
            tags: vec!["config".into()],
            attributes: det,
        }),
    )?;
    reg.register(
        "hocon_get",
        &[
            runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("path".into()), logical: types::Logicaltype::Text },
        ],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts {
            description: Some("HOCON value at a dotted path".into()),
            tags: vec!["config".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}
