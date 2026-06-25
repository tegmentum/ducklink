//! The `.prefix` dot command — SPARQL-style `prefix__name` function namespacing
//! (PLAN-prefixes). A *prefix* is a short SQL-usable alias (`jsonfns`) for an
//! opaque *expansion* (a global-identity token, `com.tegmentum.ducklink.json`).
//! Every component scalar/table/aggregate is callable both as bare `name(...)`
//! and as `prefix__name(...)` (always unambiguous). This command surfaces the
//! per-database `__ducklink_prefix*` tables: list prefixes, see the functions
//! under a prefix, inspect bare-name conflicts, and manage prefix aliases.
//!
//! All state lives in the user db (`__ducklink_prefix`, `__ducklink_prefix_function`,
//! `__ducklink_prefix_pin`), reached through the dotcmd `spi.query` against the
//! live connection. The host stages function rows during component load and
//! flushes them onto the connection on the first `spi.query` (so `.prefix list`
//! sees freshly-loaded functions). The schema is created here on demand.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });
use duckdb::dotcmd::spi;
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};

struct Component;
const FID_PREFIX: u64 = 1; // bare ".prefix" / ".prefix <verb> ..."

fn plain(text: std::string::String) -> InvokeResult {
    InvokeResult {
        text,
        state_deltas: vec![],
    }
}

/// Escape a SQL string literal (double single-quotes).
fn lit(s: &str) -> std::string::String {
    s.replace('\'', "''")
}

/// Ensure the `__ducklink_prefix*` tables exist (idempotent). The host also
/// creates them when it flushes staged rows, but `.prefix` may run before any
/// prefixed registration, so we create them here too.
fn ensure_schema() -> Result<(), std::string::String> {
    spi::query(
        "CREATE TABLE IF NOT EXISTS __ducklink_prefix(\
           name VARCHAR PRIMARY KEY, expansion VARCHAR NOT NULL, description VARCHAR,\
           created_at BIGINT NOT NULL, last_used_at BIGINT);\
         CREATE TABLE IF NOT EXISTS __ducklink_prefix_function(\
           expansion VARCHAR NOT NULL, function_name VARCHAR NOT NULL, extension_name VARCHAR,\
           shape VARCHAR NOT NULL, n_args INTEGER NOT NULL, registered_at BIGINT NOT NULL,\
           PRIMARY KEY (expansion, function_name, shape, n_args));\
         CREATE TABLE IF NOT EXISTS __ducklink_prefix_pin(\
           function_name VARCHAR NOT NULL, shape VARCHAR NOT NULL, n_args INTEGER NOT NULL,\
           expansion VARCHAR NOT NULL, set_at BIGINT NOT NULL,\
           PRIMARY KEY (function_name, shape, n_args));",
    )
    .map(|_| ())
}

const HELP: &str = "\
.prefix — SPARQL-style prefix__name function namespacing\n\
\n\
  .prefix list                       prefixes: name | expansion | description | last_used\n\
  .prefix functions <prefix>         functions registered under <prefix>'s expansion\n\
  .prefix expansion <prefix>         the expansion <prefix> resolves to\n\
  .prefix conflicts                  bare-name ambiguities (>1 expansion, same name+arity)\n\
  .prefix add <name> <expansion> [description]   register a prefix alias\n\
  .prefix rename <old> <new>         rename a prefix alias\n\
  .prefix modify <name> <description>  set a prefix's description\n\
  .prefix delete <name>              drop a prefix alias (functions persist)\n\
  .prefix verify                     consistency check of the prefix tables\n\
  .prefix prefer <name> <expansion>  (stub) pin the bare-name owner — see note\n\
  .prefix unprefer <name>            (stub) remove a bare-name pin — see note\n\
  .prefix help                       this help\n\
\n\
Every component function is ALSO callable as prefix__name(...) — always\n\
unambiguous — while bare name(...) keeps working (last-loaded wins on a\n\
collision; see .prefix conflicts).\n";

impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        vec![CommandSpec {
            id: FID_PREFIX,
            name: "prefix".into(),
            summary: "Function prefixes: prefix__name namespacing + collision view".into(),
            usage: "prefix [list|functions|expansion|conflicts|add|rename|modify|delete|verify|prefer|unprefer]".into(),
        }]
    }

    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        let _ = id;
        let arg = args.trim();
        let (verb, rest) = match arg.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (arg, ""),
        };
        ensure_schema()?;
        match verb {
            "" | "help" => Ok(plain(HELP.to_string())),
            "list" => {
                let out = spi::query(
                    "SELECT name, expansion, coalesce(description, ''), coalesce(last_used_at::VARCHAR, '') \
                     FROM __ducklink_prefix ORDER BY name",
                )?;
                Ok(plain(if out.trim().is_empty() {
                    "(no prefixes registered)\n".to_string()
                } else {
                    out
                }))
            }
            "functions" => {
                if rest.is_empty() {
                    return Err("usage: .prefix functions <prefix>".into());
                }
                let out = spi::query(&format!(
                    "SELECT f.function_name, f.shape, f.n_args, f.extension_name \
                     FROM __ducklink_prefix_function f \
                     JOIN __ducklink_prefix p ON p.expansion = f.expansion \
                     WHERE p.name = '{}' \
                     ORDER BY f.function_name, f.shape, f.n_args",
                    lit(rest)
                ))?;
                Ok(plain(if out.trim().is_empty() {
                    format!("(no functions under prefix '{rest}', or unknown prefix)\n")
                } else {
                    out
                }))
            }
            "expansion" => {
                if rest.is_empty() {
                    return Err("usage: .prefix expansion <prefix>".into());
                }
                let out = spi::query(&format!(
                    "SELECT expansion FROM __ducklink_prefix WHERE name = '{}'",
                    lit(rest)
                ))?;
                Ok(plain(if out.trim().is_empty() {
                    format!("(no such prefix: {rest})\n")
                } else {
                    out
                }))
            }
            "conflicts" => {
                // Bare-name ambiguities: same (function_name, shape, n_args) but
                // registered from >1 distinct expansion.
                let out = spi::query(
                    "SELECT function_name, shape, n_args, count(DISTINCT expansion) AS n_impls, \
                            string_agg(DISTINCT expansion, ', ') AS expansions \
                     FROM __ducklink_prefix_function \
                     GROUP BY function_name, shape, n_args \
                     HAVING count(DISTINCT expansion) > 1 \
                     ORDER BY function_name, shape, n_args",
                )?;
                Ok(plain(if out.trim().is_empty() {
                    "(no bare-name conflicts)\n".to_string()
                } else {
                    out
                }))
            }
            "add" => {
                let (name, tail) = match rest.split_once(char::is_whitespace) {
                    Some((n, t)) => (n, t.trim()),
                    None => (rest, ""),
                };
                if name.is_empty() || tail.is_empty() {
                    return Err("usage: .prefix add <name> <expansion> [description]".into());
                }
                let (expansion, desc) = match tail.split_once(char::is_whitespace) {
                    Some((e, d)) => (e, d.trim()),
                    None => (tail, ""),
                };
                let desc_sql = if desc.is_empty() {
                    "NULL".to_string()
                } else {
                    format!("'{}'", lit(desc))
                };
                spi::query(&format!(
                    "INSERT INTO __ducklink_prefix(name, expansion, description, created_at) \
                     VALUES ('{}', '{}', {}, (epoch_ms(now())/1000)::BIGINT) \
                     ON CONFLICT (name) DO UPDATE SET expansion = excluded.expansion, \
                       description = excluded.description",
                    lit(name),
                    lit(expansion),
                    desc_sql
                ))?;
                Ok(plain(format!("prefix '{name}' -> '{expansion}'\n")))
            }
            "rename" => {
                let (old, new) = match rest.split_once(char::is_whitespace) {
                    Some((o, n)) => (o, n.trim()),
                    None => (rest, ""),
                };
                if old.is_empty() || new.is_empty() {
                    return Err("usage: .prefix rename <old> <new>".into());
                }
                spi::query(&format!(
                    "UPDATE __ducklink_prefix SET name = '{}', last_used_at = (epoch_ms(now())/1000)::BIGINT \
                     WHERE name = '{}'",
                    lit(new),
                    lit(old)
                ))?;
                Ok(plain(format!("renamed prefix '{old}' -> '{new}'\n")))
            }
            "modify" => {
                let (name, desc) = match rest.split_once(char::is_whitespace) {
                    Some((n, d)) => (n, d.trim()),
                    None => (rest, ""),
                };
                if name.is_empty() {
                    return Err("usage: .prefix modify <name> <description>".into());
                }
                spi::query(&format!(
                    "UPDATE __ducklink_prefix SET description = '{}', last_used_at = (epoch_ms(now())/1000)::BIGINT \
                     WHERE name = '{}'",
                    lit(desc),
                    lit(name)
                ))?;
                Ok(plain(format!("updated description for prefix '{name}'\n")))
            }
            "delete" => {
                if rest.is_empty() {
                    return Err("usage: .prefix delete <name>".into());
                }
                spi::query(&format!(
                    "DELETE FROM __ducklink_prefix WHERE name = '{}'",
                    lit(rest)
                ))?;
                Ok(plain(format!(
                    "deleted prefix alias '{rest}' (expansion-keyed functions persist)\n"
                )))
            }
            "verify" => {
                // Function rows whose expansion has no prefix alias row.
                let orphans = spi::query(
                    "SELECT DISTINCT f.expansion \
                     FROM __ducklink_prefix_function f \
                     LEFT JOIN __ducklink_prefix p ON p.expansion = f.expansion \
                     WHERE p.name IS NULL ORDER BY f.expansion",
                )?;
                let mut report = std::string::String::new();
                if orphans.trim().is_empty() {
                    report.push_str("ok: every function expansion has a prefix alias\n");
                } else {
                    report.push_str("expansions with functions but NO prefix alias:\n");
                    report.push_str(orphans.trim_end());
                    report.push('\n');
                }
                Ok(plain(report))
            }
            "prefer" | "unprefer" => {
                // The pin re-registers the bare name against the pinned impl,
                // which requires host cooperation (the host owns the core
                // registration path). v1 records the pin row but the host does
                // not yet honor it at load — documented as a known limitation.
                let (name, expansion) = match rest.split_once(char::is_whitespace) {
                    Some((n, e)) => (n, e.trim()),
                    None => (rest, ""),
                };
                if verb == "prefer" {
                    if name.is_empty() || expansion.is_empty() {
                        return Err("usage: .prefix prefer <function_name> <expansion>".into());
                    }
                    // Record the pin for the scalar 1-arg shape as a best-effort
                    // default; full shape/arity selection is a v1.1 item.
                    spi::query(&format!(
                        "INSERT INTO __ducklink_prefix_pin(function_name, shape, n_args, expansion, set_at) \
                         VALUES ('{}', 'scalar', 1, '{}', (epoch_ms(now())/1000)::BIGINT) \
                         ON CONFLICT (function_name, shape, n_args) DO UPDATE SET expansion = excluded.expansion",
                        lit(name),
                        lit(expansion)
                    ))?;
                    Ok(plain(format!(
                        "pinned bare '{name}' -> '{expansion}' (recorded; NOTE: the host does not \
                         yet re-register the bare name from the pin — v1.1).\n"
                    )))
                } else {
                    if name.is_empty() {
                        return Err("usage: .prefix unprefer <function_name>".into());
                    }
                    spi::query(&format!(
                        "DELETE FROM __ducklink_prefix_pin WHERE function_name = '{}'",
                        lit(name)
                    ))?;
                    Ok(plain(format!("removed pin for '{name}'\n")))
                }
            }
            other => Err(format!(
                "unknown .prefix subcommand '{other}' (try: list, functions, expansion, \
                 conflicts, add, rename, modify, delete, verify, prefer, unprefer, help)"
            )),
        }
    }
}

export!(Component);
