//! Public-suffix-aware URL/domain parsing as DuckDB scalars (via the `psl`
//! crate, which embeds the Mozilla Public Suffix List). Distinct from
//! ducklink's `url`/`idna`/`urlpattern` extensions in that it understands the
//! registrable-domain (eTLD+1) boundary:
//!   registrable_domain(host_or_url) -> 'example.co.uk'
//!   public_suffix(host_or_url)      -> 'co.uk'
//!   subdomain(host_or_url)          -> 'a.b'  ('' if none)
//!   domain_label(host_or_url)       -> 'example'
//! Each accepts a bare host or a full URL (scheme/path/port/userinfo stripped).
//! NULL on bad input; never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
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
            name: "netquack".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

/// Extracts a bare host from a bare host OR a full URL. Strips scheme,
/// userinfo, port, path/query/fragment, trailing dot, and lowercases.
/// Returns None if nothing host-like remains.
fn extract_host(input: &str) -> Option<std::string::String> {
    let mut s = input.trim();
    // Strip scheme: "scheme://rest" or "scheme:rest".
    if let Some(pos) = s.find("://") {
        s = &s[pos + 3..];
    } else if let Some(pos) = s.find(':') {
        // Only treat as a scheme if the prefix looks like one (alpha + alnum/+-.).
        let scheme = &s[..pos];
        if !scheme.is_empty()
            && scheme.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            // Avoid mistaking "host:port" for "scheme:rest": a port is all digits.
            && !s[pos + 1..].chars().take_while(|c| *c != '/').all(|c| c.is_ascii_digit())
        {
            s = &s[pos + 1..];
        }
    }
    // Strip leading slashes left from scheme-relative or malformed input.
    s = s.trim_start_matches('/');
    // Cut at the first path/query/fragment delimiter -> authority.
    let authority = s.split(|c| c == '/' || c == '?' || c == '#').next().unwrap_or("");
    // Strip userinfo "user:pass@host".
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    // Strip port. IPv6 literals are bracketed; netquack does not resolve those.
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        // [::1]:port or [::1] -> not a registrable domain; let psl reject it.
        stripped.split(']').next().unwrap_or("")
    } else {
        hostport.rsplit(':').last().unwrap_or(hostport)
    };
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() { return None; }
    Some(host.to_ascii_lowercase())
}

/// Returns (registrable_domain, public_suffix, host) for a host/URL.
/// registrable_domain is None when the host has no eTLD+1 (e.g. a bare TLD,
/// an IP literal, or unparseable input).
fn parse(input: &str) -> Option<(std::string::String, std::string::String, std::string::String)> {
    let host = extract_host(input)?;
    let domain = psl::domain(host.as_bytes())?;
    let suffix = std::str::from_utf8(domain.suffix().as_bytes()).ok()?.to_string();
    let registrable = std::str::from_utf8(domain.as_bytes()).ok()?.to_string();
    Some((registrable, suffix, host))
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let input = match text_arg(&args, 0) {
            Some(s) => s,
            None => return Ok(types::Duckvalue::Null), // NULL / non-text input -> NULL
        };
        let parsed = parse(&input);
        Ok(match which {
            F::Registrable => match parsed {
                Some((reg, _, _)) => types::Duckvalue::Text(reg.into()),
                None => types::Duckvalue::Null,
            },
            F::Suffix => match parsed {
                Some((_, suf, _)) => types::Duckvalue::Text(suf.into()),
                None => types::Duckvalue::Null,
            },
            F::Subdomain => match parsed {
                // labels left of the registrable domain; '' if none.
                Some((reg, _, host)) => {
                    let sub = host.strip_suffix(&reg)
                        .map(|p| p.trim_end_matches('.'))
                        .unwrap_or("");
                    types::Duckvalue::Text(sub.to_string().into())
                }
                None => types::Duckvalue::Null,
            },
            F::DomainLabel => match parsed {
                // registrable name without its suffix: 'example.co.uk' -> 'example'
                Some((reg, suf, _)) => {
                    let label = reg.strip_suffix(&suf)
                        .map(|p| p.trim_end_matches('.'))
                        .unwrap_or(&reg);
                    types::Duckvalue::Text(label.to_string().into())
                }
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("netquack: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("netquack: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("netquack: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("netquack: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let defs = [
        ("registrable_domain", F::Registrable, "host or URL -> eTLD+1 (registrable domain)"),
        ("public_suffix", F::Suffix, "host or URL -> effective TLD (public suffix)"),
        ("subdomain", F::Subdomain, "host or URL -> labels left of the registrable domain"),
        ("domain_label", F::DomainLabel, "host or URL -> registrable name without its suffix"),
    ];
    for (name, f, desc) in defs {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        reg.register(
            name,
            &[runtime::Funcarg { name: Some("host_or_url".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Text,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["networking".into()], attributes: det }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum F { Registrable, Suffix, Subdomain, DomainLabel }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
