//! Extract structure from SQL text as DuckDB scalars (via `sqlparser` /
//! sqlparser-rs, GenericDialect):
//!   sql_tables(sql) -> json array of referenced table names (text),
//!   sql_is_valid(sql) -> boolean,
//!   sql_statement_type(sql) -> text ('SELECT'/'INSERT'/...).
//! Parse error -> NULL (tables / statement_type) or false (is_valid). Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use sqlparser::ast::{Statement, Visit, Visitor};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::ops::ControlFlow;

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "parsertools".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn parse(sql: &str) -> Option<std::vec::Vec<Statement>> {
    Parser::parse_sql(&GenericDialect {}, sql).ok()
}

/// Visitor that collects every relation (ObjectName) the AST refers to,
/// in first-seen order, de-duplicated. `visit_relations` already walks
/// FROM, JOINs, subqueries, CTEs, etc.
struct Relations {
    seen: std::vec::Vec<std::string::String>,
}
impl Visitor for Relations {
    type Break = ();
    fn pre_visit_relation(&mut self, relation: &sqlparser::ast::ObjectName) -> ControlFlow<()> {
        let name = relation
            .0
            .iter()
            .map(|p| p.to_string())
            .collect::<std::vec::Vec<_>>()
            .join(".");
        if !self.seen.contains(&name) {
            self.seen.push(name);
        }
        ControlFlow::Continue(())
    }
}

fn collect_tables(sql: &str) -> Option<std::string::String> {
    let stmts = parse(sql)?;
    let mut r = Relations { seen: std::vec::Vec::new() };
    // The derived Visit impl surfaces every relation (FROM/JOIN/subquery/CTE
    // and INSERT/UPDATE/DELETE targets) via pre_visit_relation.
    for st in &stmts {
        let _: ControlFlow<()> = st.visit(&mut r);
    }
    serde_json::to_string(&r.seen).ok()
}

fn statement_type(sql: &str) -> Option<std::string::String> {
    let stmts = parse(sql)?;
    let st = stmts.first()?;
    let kind = match st {
        Statement::Query(_) => "SELECT",
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::CreateTable(_)
        | Statement::CreateView { .. }
        | Statement::CreateIndex(_)
        | Statement::CreateSchema { .. }
        | Statement::CreateDatabase { .. } => "CREATE",
        Statement::Drop { .. } => "DROP",
        Statement::AlterTable { .. } => "ALTER",
        Statement::Truncate { .. } => "TRUNCATE",
        Statement::Explain { .. } => "EXPLAIN",
        Statement::Set(_) => "SET",
        Statement::ShowTables { .. } | Statement::ShowColumns { .. } => "SHOW",
        other => {
            // Fall back to the first token of the Display form, uppercased.
            let s = other.to_string();
            return Some(
                s.split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_uppercase()
                    .into(),
            );
        }
    };
    Some(kind.into())
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
        let sql = text_arg(&args, 0);
        Ok(match which {
            P::Tables => match sql.and_then(|s| collect_tables(&s)) {
                Some(j) => types::Duckvalue::Text(j.into()),
                None => types::Duckvalue::Null,
            },
            P::IsValid => match sql {
                Some(s) => types::Duckvalue::Boolean(parse(&s).is_some()),
                None => types::Duckvalue::Null,
            },
            P::StmtType => match sql.and_then(|s| statement_type(&s)) {
                Some(t) => types::Duckvalue::Text(t.into()),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("parsertools: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("parsertools: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("parsertools: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("parsertools: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let specs = [
        ("sql_tables", P::Tables, types::Logicaltype::Text, "JSON array of table names referenced in the SQL"),
        ("sql_is_valid", P::IsValid, types::Logicaltype::Boolean, "does the SQL parse under the generic dialect"),
        ("sql_statement_type", P::StmtType, types::Logicaltype::Text, "kind of the first statement (SELECT/INSERT/...)"),
    ];
    for (name, p, ret, desc) in specs {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, p);
        reg.register(
            name,
            &[runtime::Funcarg { name: Some("sql".into()), logical: types::Logicaltype::Text }],
            ret,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["sql".into()], attributes: det }),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum P { Tables, IsValid, StmtType }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, P>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, P>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
