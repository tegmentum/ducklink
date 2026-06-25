//! File content-type sniffing from magic bytes (libmagic-style) as DuckDB
//! scalar functions, backed by the pure-Rust, wasm-clean `infer` crate:
//!
//!   magic_mime(data BLOB)          -> VARCHAR   e.g. 'image/png', 'application/pdf'
//!   magic_extension(data BLOB)     -> VARCHAR   e.g. 'png', 'pdf'
//!   magic_matcher_type(data BLOB)  -> VARCHAR   infer matcher class:
//!                                                image/video/audio/archive/
//!                                                document/book/font/text/...
//!   is_image(data BLOB)            -> BOOLEAN
//!
//! When `infer` cannot determine the type, sniffing functions return NULL
//! (never panic). `is_image` returns a definite true/false on any non-NULL
//! input, and NULL only when the argument itself is SQL NULL.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "magic".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }

    fn reconfigure(_keys: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }

    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

// ---- Arg helpers ----

/// Extract the BLOB at position `i`. Accepts TEXT too (sniff raw UTF-8 bytes).
/// Returns `None` when the argument is SQL NULL or absent, which propagates as
/// a NULL result.
fn opt_bytes(args: &[types::Duckvalue], i: usize) -> Option<Vec<u8>> {
    match args.get(i) {
        Some(types::Duckvalue::Blob(b)) => Some(b.clone()),
        Some(types::Duckvalue::Text(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

// ---- Dispatch ----

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, args) in rows.into_iter().enumerate() {
            let row_ctx = types::Invokeinfo {
                rowindex: Some(base + i as u64),
                iswindow: ctx.iswindow,
            };
            out.push(Self::call_scalar(handle, args, row_ctx)?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = scalar_handlers()
            .lock()
            .expect("scalar handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;

        // SQL NULL in -> NULL out (except is_image, which is total on bytes).
        let bytes = opt_bytes(&args, 0);

        Ok(match which {
            ScalarHandler::Mime => match bytes.as_deref().and_then(infer::get) {
                Some(kind) => types::Duckvalue::Text(kind.mime_type().into()),
                None => types::Duckvalue::Null,
            },
            ScalarHandler::Extension => match bytes.as_deref().and_then(infer::get) {
                Some(kind) => types::Duckvalue::Text(kind.extension().into()),
                None => types::Duckvalue::Null,
            },
            ScalarHandler::MatcherType => match bytes.as_deref().and_then(infer::get) {
                Some(kind) => types::Duckvalue::Text(matcher_type_str(kind.matcher_type()).into()),
                None => types::Duckvalue::Null,
            },
            ScalarHandler::IsImage => match bytes {
                Some(b) => types::Duckvalue::Boolean(infer::is_image(&b)),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("magic: no table functions".into()))
    }

    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("magic: no aggregates".into()))
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("magic: no pragmas".into()))
    }

    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("magic: no casts".into()))
    }
}

export!(Extension);

fn matcher_type_str(m: infer::MatcherType) -> &'static str {
    use infer::MatcherType::*;
    match m {
        App => "app",
        Archive => "archive",
        Audio => "audio",
        Book => "book",
        Doc => "doc",
        Font => "font",
        Image => "image",
        Text => "text",
        Video => "video",
        Custom => "custom",
    }
}

// ---- Registration ----

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ))
        }
    };

    register_one(
        &registry,
        "magic_mime",
        types::Logicaltype::Text,
        "sniff MIME content-type from magic bytes",
        ScalarHandler::Mime,
    )?;
    register_one(
        &registry,
        "magic_extension",
        types::Logicaltype::Text,
        "sniff canonical file extension from magic bytes",
        ScalarHandler::Extension,
    )?;
    register_one(
        &registry,
        "magic_matcher_type",
        types::Logicaltype::Text,
        "sniff matcher class (image/video/audio/archive/doc/...) from magic bytes",
        ScalarHandler::MatcherType,
    )?;
    register_one(
        &registry,
        "is_image",
        types::Logicaltype::Boolean,
        "true if the bytes look like a known image format",
        ScalarHandler::IsImage,
    )?;
    Ok(())
}

fn register_one(
    registry: &runtime::ScalarRegistry,
    name: &str,
    returns: types::Logicaltype,
    description: &str,
    handler: ScalarHandler,
) -> Result<(), types::Duckerror> {
    let handle = NEXT_SCALAR_HANDLE.fetch_add(1, Ordering::Relaxed);
    scalar_handlers()
        .lock()
        .expect("scalar handler mutex poisoned")
        .insert(handle, handler);

    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("data".into()),
        logical: types::Logicaltype::Blob,
    }];
    let opts = runtime::Funcopts {
        description: Some(description.into()),
        tags: vec!["magic".into()],
        attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
    };
    registry.register(name, &args, &returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Mime,
    Extension,
    MatcherType,
    IsImage,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
