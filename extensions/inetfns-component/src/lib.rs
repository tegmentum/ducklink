//! DuckDB `inet` scalar surface, reimplemented as ducklink scalars (via
//! `ipnetwork` / `std::net`). INET is represented as VARCHAR `'addr/prefix'`
//! (a bare address means a full-width prefix: /32 for v4, /128 for v6).
//!   host(inet)        -> varchar  (address, no prefix)
//!   family(inet)      -> integer  (4 or 6)
//!   netmask(inet)     -> varchar  ('255.255.255.0/24')
//!   network(inet)     -> varchar  ('192.168.0.0/24', host bits cleared)
//!   broadcast(inet)   -> varchar  ('192.168.0.255/24')
//!   inet_contains(network, ip) -> boolean   (DuckDB `>>=` / `<<=`)
//! NULL / invalid input -> NULL (inet_contains -> NULL). Never panics.
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use ipnetwork::IpNetwork;

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "inetfns".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

/// A parsed INET value: the (unmasked) address plus its prefix length.
struct Inet { addr: IpAddr, prefix: u8 }

/// Parse `'addr'` or `'addr/prefix'`; bare address gets a full-width prefix.
fn parse_inet(s: &str) -> Option<Inet> {
    let s = s.trim();
    match s.split_once('/') {
        Some((a, p)) => {
            let addr: IpAddr = a.parse().ok()?;
            let prefix: u8 = p.parse().ok()?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            if prefix > max { return None; }
            Some(Inet { addr, prefix })
        }
        None => {
            let addr: IpAddr = s.parse().ok()?;
            let prefix = if addr.is_ipv4() { 32 } else { 128 };
            Some(Inet { addr, prefix })
        }
    }
}

fn inet_arg(args: &[types::Duckvalue], i: usize) -> Option<Inet> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => parse_inet(s), _ => None }
}

/// Build the netmask address for a v4/v6 prefix length.
fn netmask_addr(is_v4: bool, prefix: u8) -> IpAddr {
    if is_v4 {
        let bits: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
        IpAddr::V4(Ipv4Addr::from(bits))
    } else {
        let bits: u128 = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix as u32) };
        IpAddr::V6(Ipv6Addr::from(bits))
    }
}

fn network_addr(addr: &IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let mask: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
            IpAddr::V4(Ipv4Addr::from(u32::from(*a) & mask))
        }
        IpAddr::V6(a) => {
            let mask: u128 = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix as u32) };
            IpAddr::V6(Ipv6Addr::from(u128::from(*a) & mask))
        }
    }
}

fn broadcast_addr(addr: &IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let hostmask: u32 = if prefix == 0 { u32::MAX } else if prefix == 32 { 0 } else { u32::MAX >> prefix as u32 };
            IpAddr::V4(Ipv4Addr::from(u32::from(*a) | hostmask))
        }
        IpAddr::V6(a) => {
            let hostmask: u128 = if prefix == 0 { u128::MAX } else if prefix == 128 { 0 } else { u128::MAX >> prefix as u32 };
            IpAddr::V6(Ipv6Addr::from(u128::from(*a) | hostmask))
        }
    }
}

/// Does `net` (network/prefix) contain `ip`? Both must be the same family.
fn contains(net: &Inet, ip: &Inet) -> Option<bool> {
    if net.addr.is_ipv4() != ip.addr.is_ipv4() { return Some(false); }
    let cidr = IpNetwork::new(net.addr, net.prefix).ok()?;
    Some(cidr.contains(ip.addr))
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
            F::Host => match inet_arg(&args, 0) {
                Some(n) => types::Duckvalue::Text(n.addr.to_string().into()),
                None => types::Duckvalue::Null,
            },
            F::Family => match inet_arg(&args, 0) {
                Some(n) => types::Duckvalue::Int64(if n.addr.is_ipv4() { 4 } else { 6 }),
                None => types::Duckvalue::Null,
            },
            F::Netmask => match inet_arg(&args, 0) {
                Some(n) => {
                    let m = netmask_addr(n.addr.is_ipv4(), n.prefix);
                    types::Duckvalue::Text(format!("{}/{}", m, n.prefix).into())
                }
                None => types::Duckvalue::Null,
            },
            F::Network => match inet_arg(&args, 0) {
                Some(n) => {
                    let net = network_addr(&n.addr, n.prefix);
                    types::Duckvalue::Text(format!("{}/{}", net, n.prefix).into())
                }
                None => types::Duckvalue::Null,
            },
            F::Broadcast => match inet_arg(&args, 0) {
                Some(n) => {
                    let b = broadcast_addr(&n.addr, n.prefix);
                    types::Duckvalue::Text(format!("{}/{}", b, n.prefix).into())
                }
                None => types::Duckvalue::Null,
            },
            F::Contains => match (inet_arg(&args, 0), inet_arg(&args, 1)) {
                (Some(net), Some(ip)) => match contains(&net, &ip) {
                    Some(b) => types::Duckvalue::Boolean(b),
                    None => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("inetfns: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("inetfns: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("inetfns: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("inetfns: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    // host/network/netmask/broadcast -> varchar; family -> integer.
    one(&reg, "host", types::Logicaltype::Text, det, F::Host)?;
    one(&reg, "family", types::Logicaltype::Int64, det, F::Family)?;
    one(&reg, "netmask", types::Logicaltype::Text, det, F::Netmask)?;
    one(&reg, "network", types::Logicaltype::Text, det, F::Network)?;
    one(&reg, "broadcast", types::Logicaltype::Text, det, F::Broadcast)?;
    // inet_contains(network, ip) -> boolean  (the `<<=` / `>>=` operator form).
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Contains);
    reg.register("inet_contains", &[
        runtime::Funcarg { name: Some("network".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("ip".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Boolean, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("does network contain ip".into()), tags: vec!["network".into()], attributes: det }))?;
    Ok(())
}

fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, f: F) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, f);
    reg.register(name, &[runtime::Funcarg { name: Some("inet".into()), logical: types::Logicaltype::Text }],
        ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("inet".into()), tags: vec!["network".into()], attributes: attr }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum F { Host, Family, Netmask, Network, Broadcast, Contains }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }

#[cfg(test)]
mod tests {
    use super::*;

    fn inet(s: &str) -> Inet { parse_inet(s).unwrap() }

    #[test]
    fn host_strips_prefix() {
        assert_eq!(inet("192.168.0.5/24").addr.to_string(), "192.168.0.5");
        assert_eq!(inet("10.0.0.1").addr.to_string(), "10.0.0.1");
        assert_eq!(inet("2001:db8::1/64").addr.to_string(), "2001:db8::1");
    }

    #[test]
    fn family_v4_v6() {
        assert!(inet("192.168.0.5/24").addr.is_ipv4());
        assert!(!inet("2001:db8::1/64").addr.is_ipv4());
    }

    #[test]
    fn netmask_v4() {
        let n = inet("192.168.0.5/24");
        assert_eq!(netmask_addr(true, n.prefix).to_string(), "255.255.255.0");
        assert_eq!(netmask_addr(true, 16).to_string(), "255.255.0.0");
        assert_eq!(netmask_addr(true, 32).to_string(), "255.255.255.255");
        assert_eq!(netmask_addr(true, 0).to_string(), "0.0.0.0");
    }

    #[test]
    fn netmask_v6() {
        assert_eq!(netmask_addr(false, 64).to_string(), "ffff:ffff:ffff:ffff::");
        assert_eq!(netmask_addr(false, 128).to_string(), "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff");
    }

    #[test]
    fn network_clears_host_bits() {
        let n = inet("192.168.0.5/24");
        assert_eq!(network_addr(&n.addr, n.prefix).to_string(), "192.168.0.0");
        let n2 = inet("10.1.2.130/25");
        assert_eq!(network_addr(&n2.addr, n2.prefix).to_string(), "10.1.2.128");
        let v6 = inet("2001:db8::abcd/64");
        assert_eq!(network_addr(&v6.addr, v6.prefix).to_string(), "2001:db8::");
    }

    #[test]
    fn broadcast_sets_host_bits() {
        let n = inet("192.168.0.5/24");
        assert_eq!(broadcast_addr(&n.addr, n.prefix).to_string(), "192.168.0.255");
        let n2 = inet("10.1.2.130/25");
        assert_eq!(broadcast_addr(&n2.addr, n2.prefix).to_string(), "10.1.2.255");
        let host = inet("192.168.0.5/32");
        assert_eq!(broadcast_addr(&host.addr, host.prefix).to_string(), "192.168.0.5");
    }

    #[test]
    fn contains_within_and_outside() {
        assert_eq!(contains(&inet("192.168.0.0/24"), &inet("192.168.0.5")), Some(true));
        assert_eq!(contains(&inet("192.168.0.0/24"), &inet("192.168.1.5")), Some(false));
        assert_eq!(contains(&inet("10.0.0.0/8"), &inet("10.255.255.255")), Some(true));
        // mixed families never contain
        assert_eq!(contains(&inet("192.168.0.0/24"), &inet("2001:db8::1")), Some(false));
        // v6
        assert_eq!(contains(&inet("2001:db8::/32"), &inet("2001:db8:1234::1")), Some(true));
        assert_eq!(contains(&inet("2001:db8::/32"), &inet("2001:db9::1")), Some(false));
    }

    #[test]
    fn invalid_rejected() {
        assert!(parse_inet("not.an.ip").is_none());
        assert!(parse_inet("192.168.0.5/33").is_none());   // prefix too big for v4
        assert!(parse_inet("2001:db8::1/129").is_none());  // prefix too big for v6
        assert!(parse_inet("999.0.0.1").is_none());
        assert!(parse_inet("").is_none());
    }
}
