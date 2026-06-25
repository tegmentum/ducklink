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
  .prefix prefer <name> <prefix|expansion> [--shape S] [--args N]\n\
                                     pin the bare-name owner (re-registers it NOW)\n\
  .prefix unprefer <name> [--shape S] [--args N]\n\
                                     remove the pin; revert to last-loaded owner\n\
  .prefix help                       this help\n\
\n\
Every component function is ALSO callable as prefix__name(...) — always\n\
unambiguous — while bare name(...) keeps working (last-loaded wins on a\n\
collision; see .prefix conflicts).\n";

/// v1.1 THE PIN — the dotcmd<->host hook. After writing/deleting a pin row the
/// dotcmd issues this exact string via `spi.query`; the host intercepts it (it
/// never reaches the core SQL parser) and runs the apply-pins pass — re-reading
/// __ducklink_prefix_pin and re-registering the pinned bare owners against the
/// core so the pin takes effect IMMEDIATELY. Must match the host constant.
const APPLY_PINS_SENTINEL: &str = "-- ducklink:prefix apply-pins";

/// Parse trailing `--shape S` / `--args N` flags out of an argument tail,
/// returning (positional_words, shape, n_args). Unknown `--flags` are an error.
fn parse_pin_flags(
    rest: &str,
) -> Result<(Vec<&str>, Option<std::string::String>, Option<i32>), std::string::String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut shape: Option<std::string::String> = None;
    let mut n_args: Option<i32> = None;
    let mut it = rest.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        match tok {
            "--shape" => {
                let v = it.next().ok_or("--shape needs a value (scalar|table|aggregate|collation|pragma|macro)")?;
                shape = Some(v.to_lowercase());
            }
            "--args" => {
                let v = it.next().ok_or("--args needs an integer value")?;
                n_args = Some(v.parse::<i32>().map_err(|_| format!("--args: not an integer: {v}"))?);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag '{other}' (expected --shape or --args)"));
            }
            other => positional.push(other),
        }
    }
    Ok((positional, shape, n_args))
}

/// Resolve the pin target's EXPANSION. `target` may be a prefix alias (looked up
/// in __ducklink_prefix) or an expansion verbatim. Returns the expansion string.
fn resolve_expansion(target: &str) -> Result<std::string::String, std::string::String> {
    // Prefer a prefix-alias match (the common case: `.prefix prefer f jsonfns`).
    let by_prefix = spi::query(&format!(
        "SELECT expansion FROM __ducklink_prefix WHERE name = '{}' LIMIT 1",
        lit(target)
    ))?;
    let first = by_prefix.lines().next().unwrap_or("").trim();
    if !first.is_empty() {
        return Ok(first.to_string());
    }
    // Else accept the target as an expansion only if some function row uses it.
    let by_exp = spi::query(&format!(
        "SELECT DISTINCT expansion FROM __ducklink_prefix_function WHERE expansion = '{}' LIMIT 1",
        lit(target)
    ))?;
    let exp = by_exp.lines().next().unwrap_or("").trim();
    if !exp.is_empty() {
        return Ok(exp.to_string());
    }
    Err(format!(
        "'{target}' is neither a known prefix alias nor an expansion with registered functions"
    ))
}

/// Resolve (shape, n_args) for `name`: use the explicit flags if given,
/// otherwise look them up from __ducklink_prefix_function. Errors (listing the
/// options) when ambiguous across shapes/arities and nothing was specified.
fn resolve_shape_arity(
    name: &str,
    shape: Option<std::string::String>,
    n_args: Option<i32>,
) -> Result<(std::string::String, i32), std::string::String> {
    if let (Some(s), Some(a)) = (&shape, n_args) {
        return Ok((s.clone(), a));
    }
    // Gather the distinct (shape, n_args) registered for this name, filtered by
    // any partial flag the user did give.
    let mut sql = format!(
        "SELECT DISTINCT shape, n_args FROM __ducklink_prefix_function WHERE function_name = '{}'",
        lit(name)
    );
    if let Some(s) = &shape {
        sql.push_str(&format!(" AND shape = '{}'", lit(s)));
    }
    if let Some(a) = n_args {
        sql.push_str(&format!(" AND n_args = {a}"));
    }
    sql.push_str(" ORDER BY shape, n_args");
    let rows = spi::query(&sql)?;
    let opts: Vec<(std::string::String, i32)> = rows
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut c = l.split('\t');
            let s = c.next()?.to_string();
            let a = c.next()?.trim().parse::<i32>().ok()?;
            Some((s, a))
        })
        .collect();
    match opts.as_slice() {
        [] => Err(format!(
            "no registered function named '{name}'{} — load the extension first, or run a query so the host flushes the prefix tables",
            shape.as_ref().map(|s| format!(" with shape '{s}'")).unwrap_or_default()
        )),
        [(s, a)] => Ok((s.clone(), *a)),
        many => {
            let listed = many
                .iter()
                .map(|(s, a)| format!("--shape {s} --args {a}"))
                .collect::<Vec<_>>()
                .join("; ");
            Err(format!(
                "'{name}' is ambiguous across shapes/arities; specify one: {listed}"
            ))
        }
    }
}

/// `.prefix prefer <name> <prefix|expansion> [--shape S] [--args N]`.
fn prefer(rest: &str) -> Result<InvokeResult, std::string::String> {
    let (positional, shape_flag, args_flag) = parse_pin_flags(rest)?;
    let (name, target) = match positional.as_slice() {
        [n, t] => (*n, *t),
        _ => return Err("usage: .prefix prefer <name> <prefix|expansion> [--shape S] [--args N]".into()),
    };
    let expansion = resolve_expansion(target)?;
    let (shape, n_args) = resolve_shape_arity(name, shape_flag, args_flag)?;
    // (b) write/replace the pin row.
    spi::query(&format!(
        "INSERT INTO __ducklink_prefix_pin(function_name, shape, n_args, expansion, set_at) \
         VALUES ('{}', '{}', {}, '{}', (epoch_ms(now())/1000)::BIGINT) \
         ON CONFLICT (function_name, shape, n_args) DO UPDATE SET expansion = excluded.expansion, \
           set_at = excluded.set_at",
        lit(name),
        lit(&shape),
        n_args,
        lit(&expansion)
    ))?;
    // (c) trigger the host to re-register the pinned bare owner NOW.
    spi::query(APPLY_PINS_SENTINEL)?;
    Ok(plain(format!(
        "pinned bare '{name}' ({shape}/{n_args}-arg) -> expansion '{expansion}' (re-registered NOW)\n"
    )))
}

/// `.prefix unprefer <name> [--shape S] [--args N]`.
fn unprefer(rest: &str) -> Result<InvokeResult, std::string::String> {
    let (positional, shape_flag, args_flag) = parse_pin_flags(rest)?;
    let name = match positional.as_slice() {
        [n] => *n,
        _ => return Err("usage: .prefix unprefer <name> [--shape S] [--args N]".into()),
    };
    // Resolve from the PIN table (the function may have been unloaded), falling
    // back to the function table if no pin row narrows it.
    let mut sql = format!(
        "SELECT DISTINCT shape, n_args FROM __ducklink_prefix_pin WHERE function_name = '{}'",
        lit(name)
    );
    if let Some(s) = &shape_flag {
        sql.push_str(&format!(" AND shape = '{}'", lit(s)));
    }
    if let Some(a) = args_flag {
        sql.push_str(&format!(" AND n_args = {a}"));
    }
    let rows = spi::query(&sql)?;
    let pins: Vec<(std::string::String, i32)> = rows
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut c = l.split('\t');
            Some((c.next()?.to_string(), c.next()?.trim().parse::<i32>().ok()?))
        })
        .collect();
    if pins.is_empty() {
        return Err(format!("no pin recorded for '{name}'"));
    }
    // Delete the matching pin row(s).
    let mut del = format!(
        "DELETE FROM __ducklink_prefix_pin WHERE function_name = '{}'",
        lit(name)
    );
    if let Some(s) = &shape_flag {
        del.push_str(&format!(" AND shape = '{}'", lit(s)));
    }
    if let Some(a) = args_flag {
        del.push_str(&format!(" AND n_args = {a}"));
    }
    spi::query(&del)?;
    // Trigger the host to revert the bare name to the last-loaded owner. The
    // host's apply-pins reads the (now-smaller) pin table; for the deleted
    // key(s) it reverts to the default owner via its `unpin` logic. We signal
    // the host to revert by re-asserting the remaining pins AND the host's
    // apply pass re-registers defaults for keys no longer pinned.
    spi::query(APPLY_PINS_SENTINEL)?;
    let listed = pins
        .iter()
        .map(|(s, a)| format!("{s}/{a}-arg"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(plain(format!(
        "removed pin for '{name}' ({listed}); bare name reverts to last-loaded owner\n"
    )))
}

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
            "prefer" => prefer(rest),
            "unprefer" => unprefer(rest),
            other => Err(format!(
                "unknown .prefix subcommand '{other}' (try: list, functions, expansion, \
                 conflicts, add, rename, modify, delete, verify, prefer, unprefer, help)"
            )),
        }
    }
}

export!(Component);
