//! Chinese Hanzi -> pinyin romanization as DuckDB scalars (via the `pinyin` crate):
//!   to_pinyin(text)          -> pinyin WITH tone marks, space-separated
//!   to_pinyin_plain(text)    -> pinyin WITHOUT tone marks, space-separated
//!   to_pinyin_initials(text) -> first letter of each syllable, space-separated
//! Hanzi are romanized; any non-Hanzi character passes through unchanged.
//! SQL NULL -> NULL. Never panics; best-effort romanization otherwise.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use pinyin::ToPinyin;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "pinyin".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

#[derive(Clone, Copy, PartialEq)]
enum P { Tone, Plain, Initials }

/// Convert `input` to a space-separated pinyin string per mode. Hanzi characters
/// become their pinyin syllable; runs of non-Hanzi characters pass through
/// verbatim. Syllables and passthrough runs are separated by single spaces.
fn convert(input: &str, mode: P) -> String {
    let mut out = std::string::String::new();
    let mut pending = std::string::String::new(); // accumulated non-Hanzi run
    let mut need_sep = false; // a space is owed before the next emitted token

    let flush_pending = |out: &mut std::string::String, pending: &mut std::string::String, need_sep: &mut bool| {
        if !pending.is_empty() {
            if *need_sep { out.push(' '); }
            out.push_str(pending);
            pending.clear();
            *need_sep = true;
        }
    };

    for (ch, py) in input.chars().zip(input.to_pinyin()) {
        match py {
            Some(p) => {
                flush_pending(&mut out, &mut pending, &mut need_sep);
                if need_sep { out.push(' '); }
                let syl = match mode {
                    P::Tone => p.with_tone(),
                    P::Plain => p.plain(),
                    P::Initials => p.first_letter(),
                };
                out.push_str(syl);
                need_sep = true;
            }
            None => {
                // Non-Hanzi: accumulate as a passthrough run (keeps adjacent
                // ASCII like "a" or "123" intact rather than splitting it).
                pending.push(ch);
            }
        }
    }
    flush_pending(&mut out, &mut pending, &mut need_sep);
    out.into()
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
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
        // NULL in -> NULL out; otherwise best-effort romanization.
        Ok(match text_arg(&args, 0) {
            Some(text) => types::Duckvalue::Text(convert(&text, which)),
            None => types::Duckvalue::Null,
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("pinyin: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("pinyin: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("pinyin: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("pinyin: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    for (name, mode, desc) in [
        ("to_pinyin", P::Tone, "Hanzi -> pinyin with tone marks (space-separated)"),
        ("to_pinyin_plain", P::Plain, "Hanzi -> pinyin without tone marks (space-separated)"),
        ("to_pinyin_initials", P::Initials, "Hanzi -> first letter of each syllable (space-separated)"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, mode);
        reg.register(name,
            &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["text".into()], attributes: det }))?;
    }
    Ok(())
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, P>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, P>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
