//! iCalendar (.ics) parsing as DuckDB scalars (via the `ical` crate's
//! IcalParser over the input bytes):
//!   ical_event_count(ics) -> bigint  -- number of VEVENTs across all VCALENDARs
//!   ical_to_json(ics)     -> json    -- [{summary,dtstart,dtend,uid}, ...]
//!   ical_summaries(ics)   -> json    -- ["summary", ...]
//! Parse error / non-text input -> NULL. Never panics.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use ical::IcalParser;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ical".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

/// Parse every VCALENDAR in `ics`, returning all VEVENTs flattened.
/// Any parse error in any calendar block -> None (whole call yields NULL).
fn parse_events(ics: &str) -> Option<std::vec::Vec<ical::parser::ical::component::IcalEvent>> {
    let mut events = std::vec::Vec::new();
    for cal in IcalParser::new(ics.as_bytes()) {
        let cal = cal.ok()?;
        events.extend(cal.events);
    }
    Some(events)
}

/// First value of a named property on an event, if present.
fn prop<'a>(ev: &'a ical::parser::ical::component::IcalEvent, name: &str) -> Option<&'a str> {
    ev.properties.iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .and_then(|p| p.value.as_deref())
}

fn events_to_json(events: &[ical::parser::ical::component::IcalEvent]) -> Option<std::string::String> {
    let arr: std::vec::Vec<serde_json::Value> = events.iter().map(|ev| {
        let mut obj = serde_json::Map::new();
        for key in ["summary", "dtstart", "dtend", "uid"] {
            if let Some(v) = prop(ev, key) {
                obj.insert(key.to_string(), serde_json::Value::String(v.to_string()));
            }
        }
        serde_json::Value::Object(obj)
    }).collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
}

fn summaries_to_json(events: &[ical::parser::ical::component::IcalEvent]) -> Option<std::string::String> {
    let arr: std::vec::Vec<serde_json::Value> = events.iter()
        .filter_map(|ev| prop(ev, "summary").map(|s| serde_json::Value::String(s.to_string())))
        .collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
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
        let s = match text(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let events = match parse_events(&s) { Some(e) => e, None => return Ok(types::Duckvalue::Null) };
        Ok(match handle {
            1 => types::Duckvalue::Int64(events.len() as i64),
            2 => match events_to_json(&events) { Some(j) => types::Duckvalue::Text(j.into()), None => types::Duckvalue::Null },
            3 => match summaries_to_json(&events) { Some(j) => types::Duckvalue::Text(j.into()), None => types::Duckvalue::Null },
            _ => types::Duckvalue::Null,
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ical: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ical: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ical: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ical: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let arg = |name: &str| runtime::Funcarg { name: Some(name.into()), logical: types::Logicaltype::Text };
    reg.register("ical_event_count", &[arg("ics")],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("number of VEVENTs in an iCalendar string".into()), tags: vec!["ical".into()], attributes: det }))?;
    reg.register("ical_to_json", &[arg("ics")],
        types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("iCalendar VEVENTs -> JSON array of {summary,dtstart,dtend,uid}".into()), tags: vec!["ical".into()], attributes: det }))?;
    reg.register("ical_summaries", &[arg("ics")],
        types::Logicaltype::Text, runtime::ScalarCallback::new(3),
        Some(&runtime::Funcopts { description: Some("iCalendar VEVENT SUMMARYs -> JSON array".into()), tags: vec!["ical".into()], attributes: det }))?;
    Ok(())
}
