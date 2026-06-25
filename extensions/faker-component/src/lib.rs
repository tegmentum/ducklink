//! Fake data generation as DuckDB scalars (via `fake`, bundled data lists):
//!   fake_name(), fake_email(), fake_username(), fake_city(), fake_company().
//!   Nondeterministic (wasi random). Useful for seeding test datasets.
use fake::Fake;
use fake::faker::name::en::Name;
use fake::faker::internet::en::{SafeEmail, Username};
use fake::faker::address::en::CityName;
use fake::faker::company::en::CompanyName;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "faker".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, _args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let s: std::string::String = match handle {
            1 => Name().fake(),
            2 => SafeEmail().fake(),
            3 => Username().fake(),
            4 => CityName().fake(),
            5 => CompanyName().fake(),
            _ => return Err(types::Duckerror::Internal("unknown scalar handle".into())),
        };
        Ok(types::Duckvalue::Text(s.into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("faker: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("faker: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("faker: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("faker: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let nondet = types::Funcflags::empty();
    for (name, cb, desc) in [
        ("fake_name", 1u32, "random person name"), ("fake_email", 2, "random email"),
        ("fake_username", 3, "random username"), ("fake_city", 4, "random city"),
        ("fake_company", 5, "random company name"),
    ] {
        reg.register(name, &[], &types::Logicaltype::Text, runtime::ScalarCallback::new(cb),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["fake".into()], attributes: nondet }))?;
    }
    Ok(())
}
