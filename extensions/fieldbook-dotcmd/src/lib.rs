//! Fieldbook authoring + orchestration dot commands, as a pluggable ducklink
//! CLI component. Ports the `.fb` / `.entry` / `.run` surface from the
//! standalone `fieldbook` binary onto the ducklink dot-command SPI.
//!
//! # SQL wiring: direct against backing tables (self-contained)
//!
//! Every command reads AND writes the `__fieldbook_*` tables DIRECTLY via
//! `spi::query`, which runs on the CLI's primary connection. This component
//! bootstraps the four backing tables itself on first use, so it does NOT
//! require the `fieldbook` wasm engine (fieldbook-component) to be loaded — no
//! `LOAD fieldbook;` needed, and no dependency on the `fieldbook_create` /
//! `fieldbook_add_entry` / `fieldbook_drop` / `fieldbook_record_run` scalars.
//!
//! Why direct SQL rather than the engine scalars: under Direction 1 of the
//! standalone ducklink CLI (see docs/nested-exec-direction-1-plan.md §5.b.1),
//! extension scalars that call `nested_exec::query` open a sibling wasm-DuckDB
//! core on the same database file. That sibling has its own buffer cache and
//! catalog view, so writes committed there do not become visible to the
//! primary connection this component is reading through. Bypassing the
//! scalars entirely sidesteps the problem: everything lives on the primary.
//!
//! The bootstrapped schema is byte-identical to the wasm engine's, so a
//! database that later ALSO gets fieldbook-component loaded (under Direction 2,
//! where nested-exec IS coherent) will share rows with it. That's a bonus, not
//! the design goal.
//!
//! # "Current fieldbook" persistence
//!
//! The CLI's `state-delta` mechanism is for CLI-side session state (display
//! mode, etc.), not per-fieldbook context. We store the current fieldbook in
//! `__fieldbook_state(key, value)` so it rides the .duckdb file across
//! sessions.

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:dotcmd/dotcmd" });

use core::sync::atomic::{AtomicBool, Ordering};

use duckdb::dotcmd::spi;
use exports::duckdb::dotcmd::registry::{CommandSpec, Guest, InvokeResult};

struct Component;

const FID_FB: u64 = 0;
const FID_ENTRY: u64 = 1;
const FID_RUN: u64 = 2;

const STATE_KEY_CURRENT: &str = "current_fieldbook";

/// Bootstrapped flag: `ensure_bootstrap` fires `CREATE TABLE IF NOT EXISTS`
/// exactly once per component instance. Repeat calls are a load-Relaxed +
/// return.
static BOOTSTRAPPED: AtomicBool = AtomicBool::new(false);

impl Guest for Component {
    fn list_commands() -> Vec<CommandSpec> {
        let c = |id, name: &str, summary: &str, usage: &str| CommandSpec {
            id,
            name: name.into(),
            summary: summary.into(),
            usage: usage.into(),
        };
        vec![
            c(
                FID_FB,
                "fb",
                "List / create / open / drop fieldbooks",
                "fb [new|open|drop <name>]",
            ),
            c(
                FID_ENTRY,
                "entry",
                "List / add / show entries in the current fieldbook",
                "entry [add <sql>|show <n>]",
            ),
            c(
                FID_RUN,
                "run",
                "Execute one entry (.run N) or every entry in order (.run)",
                "run [N]",
            ),
        ]
    }

    fn invoke(id: u64, args: String) -> Result<InvokeResult, String> {
        // Idempotent lazy bootstrap. Cached via atomic bool so the hot path is
        // a single Relaxed load. Done at first invocation rather than at load
        // time so `list_commands` stays a pure snapshot (called during host
        // boot before the extension is necessarily fully initialized).
        ensure_bootstrap()?;
        match id {
            FID_FB => cmd_fb(args.as_str()),
            FID_ENTRY => cmd_entry(args.as_str()),
            FID_RUN => cmd_run(args.as_str()),
            other => Err(std::format!("fieldbook-dotcmd: unknown command id {other}")),
        }
    }
}
export!(Component);

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

fn cmd_fb(args: &str) -> Result<InvokeResult, String> {
    let (verb, rest) = split_verb(args);
    match verb {
        "" => fb_list(),
        "new" => fb_new(rest),
        "open" => fb_open(rest),
        "drop" => fb_drop(rest),
        other => Err(std::format!(
            "unknown .fb subcommand '{other}' (try: new, open, drop)"
        )),
    }
}

fn cmd_entry(args: &str) -> Result<InvokeResult, String> {
    let (verb, rest) = split_verb(args);
    match verb {
        "" => entry_list(),
        "add" => entry_add(rest),
        "show" => entry_show(rest),
        other => Err(std::format!(
            "unknown .entry subcommand '{other}' (try: add, show)"
        )),
    }
}

fn cmd_run(args: &str) -> Result<InvokeResult, String> {
    let arg = args.trim();
    if arg.is_empty() {
        run_all()
    } else {
        let ord: i64 = arg
            .parse()
            .map_err(|_| std::format!("invalid entry ordinal '{arg}'"))?;
        run_one(ord)
    }
}

// ---------------------------------------------------------------------------
// .fb — fieldbook lifecycle
// ---------------------------------------------------------------------------

fn fb_list() -> Result<InvokeResult, String> {
    let rows = query_rows(
        "SELECT b.name, \
                coalesce(b.description, ''), \
                (SELECT count(*) FROM __fieldbook_entries e \
                   WHERE e.fieldbook = b.name), \
                b.updated_at \
         FROM __fieldbook_books b ORDER BY b.name",
    )?;
    if rows.is_empty() {
        return Ok(plain("(no fieldbooks)\n".to_string()));
    }
    let out = text_table(&["name", "description", "entries", "updated_at"], &rows);
    Ok(plain(out))
}

fn fb_new(rest: &str) -> Result<InvokeResult, String> {
    let name = require_name(rest, "fb new <name>")?;
    // Race-free check-then-insert isn't necessary here (single-connection CLI
    // usage), and INSERT ... ON CONFLICT DO NOTHING makes the second call a
    // no-op regardless, so a pre-check is only used to shape the returned
    // message.
    let existed = query_scalar_i64(&std::format!(
        "SELECT count(*) FROM __fieldbook_books WHERE name = {}",
        sql_literal(&name)
    ))? > 0;
    if existed {
        return Ok(plain(std::format!("fieldbook {name:?} already exists\n")));
    }
    spi::query(&std::format!(
        "INSERT INTO __fieldbook_books(name) VALUES ({}) ON CONFLICT DO NOTHING",
        sql_literal(&name)
    ))?;
    set_current(&name)?;
    Ok(plain(std::format!(
        "created fieldbook {name:?}; current is now {name:?}\n"
    )))
}

fn fb_open(rest: &str) -> Result<InvokeResult, String> {
    let name = require_name(rest, "fb open <name>")?;
    let count = query_scalar_i64(&std::format!(
        "SELECT count(*) FROM __fieldbook_books WHERE name = {}",
        sql_literal(&name)
    ))?;
    if count == 0 {
        return Err(std::format!("no such fieldbook {name:?}"));
    }
    set_current(&name)?;
    Ok(plain(std::format!("current fieldbook set to {name:?}\n")))
}

fn fb_drop(rest: &str) -> Result<InvokeResult, String> {
    let name = require_name(rest, "fb drop <name>")?;
    let existed = query_scalar_i64(&std::format!(
        "SELECT count(*) FROM __fieldbook_books WHERE name = {}",
        sql_literal(&name)
    ))? > 0;
    if !existed {
        return Ok(plain(std::format!("no such fieldbook {name:?}\n")));
    }
    // Order: runs -> entries -> books. FKs aren't declared on the schema but
    // this order also matches what a rehydrating fieldbook-component would
    // expect if it later took over the same file.
    spi::query(&std::format!(
        "DELETE FROM __fieldbook_runs WHERE fieldbook = {}",
        sql_literal(&name)
    ))?;
    spi::query(&std::format!(
        "DELETE FROM __fieldbook_entries WHERE fieldbook = {}",
        sql_literal(&name)
    ))?;
    spi::query(&std::format!(
        "DELETE FROM __fieldbook_books WHERE name = {}",
        sql_literal(&name)
    ))?;
    if get_current().as_deref() == Some(name.as_str()) {
        spi::query(&std::format!(
            "DELETE FROM __fieldbook_state WHERE key = {}",
            sql_literal(STATE_KEY_CURRENT)
        ))?;
    }
    Ok(plain(std::format!("dropped fieldbook {name:?}\n")))
}

// ---------------------------------------------------------------------------
// .entry — authoring
// ---------------------------------------------------------------------------

fn entry_list() -> Result<InvokeResult, String> {
    let current = require_current()?;
    let rows = query_rows(&std::format!(
        "SELECT ordinal, coalesce(title, ''), source, updated_at \
         FROM __fieldbook_entries WHERE fieldbook = {} ORDER BY ordinal",
        sql_literal(&current)
    ))?;
    if rows.is_empty() {
        return Ok(plain(std::format!(
            "(no entries in fieldbook {current:?})\n"
        )));
    }
    let out = text_table(&["ordinal", "title", "source", "updated_at"], &rows);
    Ok(plain(out))
}

fn entry_add(rest: &str) -> Result<InvokeResult, String> {
    let current = require_current()?;
    let source = rest.trim();
    if source.is_empty() {
        // TODO: add a future `spi.edit(initial: string) -> result<string, string>`
        // host import so an interactive dot command can shell out to $EDITOR
        // for multiline entries. The standalone `fieldbook` CLI does this via
        // std::process::Command; wasm's WASI ctx has no execve so we can't
        // reproduce it in-guest.
        return Err(
            ".entry add SQL — inline mode only for now; use $EDITOR outside the \
             CLI to prepare multiline SQL and paste as one line"
                .to_string(),
        );
    }
    let next_ord = query_scalar_i64(&std::format!(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM __fieldbook_entries WHERE fieldbook = {}",
        sql_literal(&current)
    ))?;
    spi::query(&std::format!(
        "INSERT INTO __fieldbook_entries(fieldbook, ordinal, source) VALUES ({}, {}, {})",
        sql_literal(&current),
        next_ord,
        sql_literal(source)
    ))?;
    // Bump parent book's updated_at so the .fb list reflects the new work.
    spi::query(&std::format!(
        "UPDATE __fieldbook_books SET updated_at = current_timestamp WHERE name = {}",
        sql_literal(&current)
    ))?;
    Ok(plain(std::format!(
        "added entry {next_ord} to {current:?}\n"
    )))
}

fn entry_show(rest: &str) -> Result<InvokeResult, String> {
    let current = require_current()?;
    let ord = parse_ordinal(rest, "entry show <n>")?;
    let raw = spi::query(&std::format!(
        "SELECT source FROM __fieldbook_entries \
         WHERE fieldbook = {} AND ordinal = {}",
        sql_literal(&current),
        ord
    ))?;
    let src = raw.trim_end_matches('\n');
    if src.is_empty() {
        return Ok(plain(std::format!("no entry {ord} in {current:?}\n")));
    }
    Ok(plain(std::format!(
        "-- entry {ord} of {current:?}\n{src}\n"
    )))
}

// ---------------------------------------------------------------------------
// .run — orchestration
// ---------------------------------------------------------------------------

fn run_all() -> Result<InvokeResult, String> {
    let current = require_current()?;
    let ordinals = list_ordinals(&current)?;
    if ordinals.is_empty() {
        return Ok(plain(std::format!(
            "(no entries in fieldbook {current:?})\n"
        )));
    }
    let run_id = next_run_id()?;
    let mut outcomes: std::vec::Vec<Outcome> = std::vec::Vec::new();
    for ord in ordinals {
        let outcome = execute_one(&current, ord, run_id)?;
        let stop = outcome.status == "error";
        outcomes.push(outcome);
        if stop {
            break;
        }
    }
    Ok(plain(format_outcomes(run_id, &outcomes)))
}

fn run_one(ord: i64) -> Result<InvokeResult, String> {
    let current = require_current()?;
    // Confirm the entry exists before spending a run_id / recording a phantom.
    let src_raw = spi::query(&std::format!(
        "SELECT source FROM __fieldbook_entries \
         WHERE fieldbook = {} AND ordinal = {}",
        sql_literal(&current),
        ord
    ))?;
    if src_raw.trim_end_matches('\n').is_empty() {
        return Err(std::format!("no entry {ord} in {current:?}"));
    }
    let run_id = next_run_id()?;
    let outcome = execute_one(&current, ord, run_id)?;
    Ok(plain(format_outcomes(run_id, &[outcome])))
}

struct Outcome {
    ordinal: i64,
    status: std::string::String,
    duration_ms: i64,
    error: std::string::String,
}

/// Read the entry source, execute it via `spi::query`, and persist the outcome
/// as an `INSERT INTO __fieldbook_runs` row on the primary. Timing is derived
/// from DuckDB's `epoch_ms(current_timestamp)`; row count isn't currently
/// surfaced by `spi::query` (which returns text), so we record -1 as the
/// "unknown" sentinel.
fn execute_one(fb: &str, ord: i64, run_id: i64) -> Result<Outcome, String> {
    let src_raw = spi::query(&std::format!(
        "SELECT source FROM __fieldbook_entries \
         WHERE fieldbook = {} AND ordinal = {}",
        sql_literal(fb),
        ord
    ))?;
    let src = src_raw.trim_end_matches('\n').to_string();
    if src.is_empty() {
        // Shouldn't happen — the caller already validated non-empty ordinals —
        // but be explicit rather than record a phantom.
        return Err(std::format!("entry {ord} of {fb:?} has empty source"));
    }

    let started = query_scalar_i64("SELECT epoch_ms(current_timestamp)").unwrap_or(0);
    let (status, error) = match spi::query(&src) {
        Ok(_) => ("ok".to_string(), std::string::String::new()),
        Err(e) => ("error".to_string(), e),
    };
    let finished = query_scalar_i64("SELECT epoch_ms(current_timestamp)").unwrap_or(started);
    let duration_ms = (finished - started).max(0);

    // Best-effort record: a record failure gets logged into the outcome as
    // status-unaffecting; the outcome the user cares about is the execution.
    let _ = spi::query(&std::format!(
        "INSERT INTO __fieldbook_runs \
             (fieldbook, ordinal, run_id, duration_ms, status, error, row_count) \
         VALUES ({}, {}, {}, {}, {}, {}, -1)",
        sql_literal(fb),
        ord,
        run_id,
        duration_ms,
        sql_literal(&status),
        sql_literal(&error)
    ));

    Ok(Outcome {
        ordinal: ord,
        status,
        duration_ms,
        error,
    })
}

fn list_ordinals(fb: &str) -> Result<std::vec::Vec<i64>, String> {
    let raw = spi::query(&std::format!(
        "SELECT ordinal FROM __fieldbook_entries \
         WHERE fieldbook = {} ORDER BY ordinal",
        sql_literal(fb)
    ))?;
    let mut ords = std::vec::Vec::new();
    for line in raw.split('\n') {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // A single-column result is the whole line with no tab, so parse it
        // directly. If the shape ever changes we'll notice via the parse error.
        let n: i64 = t
            .parse()
            .map_err(|_| std::format!("unexpected ordinal cell {t:?}"))?;
        ords.push(n);
    }
    Ok(ords)
}

fn next_run_id() -> Result<i64, String> {
    let raw = spi::query("SELECT COALESCE(MAX(run_id), 0) + 1 FROM __fieldbook_runs")?;
    let t = raw.trim();
    let n: i64 = t
        .parse()
        .map_err(|_| std::format!("unexpected run_id cell {t:?}"))?;
    Ok(n)
}

fn format_outcomes(run_id: i64, outcomes: &[Outcome]) -> std::string::String {
    let mut rows: std::vec::Vec<std::vec::Vec<std::string::String>> =
        std::vec::Vec::with_capacity(outcomes.len());
    for o in outcomes {
        let err = truncate(&o.error, 80);
        rows.push(std::vec![
            o.ordinal.to_string(),
            o.status.clone(),
            o.duration_ms.to_string(),
            err,
        ]);
    }
    let mut out = std::format!("run {run_id}:\n");
    out.push_str(&text_table(
        &["ordinal", "status", "duration_ms", "error"],
        &rows,
    ));
    if let Some(bad) = outcomes.iter().find(|o| o.status == "error") {
        out.push_str(&std::format!(
            "stopped at entry {} (error)\n",
            bad.ordinal
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// SPI helpers
// ---------------------------------------------------------------------------

/// Create the four backing tables on first use. Byte-identical to the
/// fieldbook-component engine's schema so a database that later ALSO gets the
/// wasm engine loaded shares state with it.
fn ensure_bootstrap() -> Result<(), String> {
    if BOOTSTRAPPED.load(Ordering::Relaxed) {
        return Ok(());
    }
    spi::query(
        "CREATE TABLE IF NOT EXISTS __fieldbook_books ( \
             name TEXT PRIMARY KEY, \
             description TEXT, \
             created_at TIMESTAMP DEFAULT current_timestamp, \
             updated_at TIMESTAMP DEFAULT current_timestamp \
         )",
    )?;
    spi::query(
        "CREATE TABLE IF NOT EXISTS __fieldbook_entries ( \
             fieldbook TEXT NOT NULL, \
             ordinal BIGINT NOT NULL, \
             title TEXT, \
             source TEXT NOT NULL, \
             created_at TIMESTAMP DEFAULT current_timestamp, \
             updated_at TIMESTAMP DEFAULT current_timestamp, \
             PRIMARY KEY (fieldbook, ordinal) \
         )",
    )?;
    spi::query(
        "CREATE TABLE IF NOT EXISTS __fieldbook_runs ( \
             fieldbook TEXT NOT NULL, \
             ordinal BIGINT NOT NULL, \
             run_id BIGINT NOT NULL, \
             started_at TIMESTAMP DEFAULT current_timestamp, \
             duration_ms BIGINT, \
             status TEXT, \
             error TEXT, \
             row_count BIGINT \
         )",
    )?;
    spi::query(
        "CREATE TABLE IF NOT EXISTS __fieldbook_state ( \
             key TEXT PRIMARY KEY, \
             value TEXT \
         )",
    )?;
    BOOTSTRAPPED.store(true, Ordering::Relaxed);
    Ok(())
}

fn set_current(name: &str) -> Result<(), String> {
    // INSERT OR REPLACE isn't ubiquitous across DuckDB versions; DELETE +
    // INSERT is equivalent and unambiguously supported.
    spi::query(&std::format!(
        "DELETE FROM __fieldbook_state WHERE key = {}",
        sql_literal(STATE_KEY_CURRENT)
    ))?;
    spi::query(&std::format!(
        "INSERT INTO __fieldbook_state(key, value) VALUES ({}, {})",
        sql_literal(STATE_KEY_CURRENT),
        sql_literal(name)
    ))?;
    Ok(())
}

fn get_current() -> Option<std::string::String> {
    let raw = spi::query(&std::format!(
        "SELECT value FROM __fieldbook_state WHERE key = {}",
        sql_literal(STATE_KEY_CURRENT)
    ))
    .ok()?;
    let t = raw.trim_end_matches('\n');
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn require_current() -> Result<std::string::String, String> {
    get_current().ok_or_else(|| "no current fieldbook — .fb open <name> first".to_string())
}

fn query_scalar_i64(sql: &str) -> Result<i64, String> {
    let raw = spi::query(sql)?;
    let t = raw.trim();
    t.parse::<i64>()
        .map_err(|_| std::format!("expected i64 from `{sql}`, got {t:?}"))
}

/// Run a SELECT and return rows as `Vec<Vec<String>>`. Empty result is an
/// empty vec. NULL cells arrive as empty strings (spi's documented contract).
fn query_rows(sql: &str) -> Result<std::vec::Vec<std::vec::Vec<std::string::String>>, String> {
    let raw = spi::query(sql)?;
    Ok(parse_rows(&raw))
}

fn parse_rows(text: &str) -> std::vec::Vec<std::vec::Vec<std::string::String>> {
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        return std::vec::Vec::new();
    }
    trimmed
        .split('\n')
        .map(|row| row.split('\t').map(|c| c.to_string()).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Single-quote-escape a SQL text literal (same helper shape as the standalone
/// CLI's `orchestration.rs::quote`).
fn sql_literal(s: &str) -> std::string::String {
    let mut out = std::string::String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Split "verb rest" on the first whitespace. Empty verb + empty rest on empty
/// input.
fn split_verb(args: &str) -> (&str, &str) {
    let trimmed = args.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (trimmed, ""),
    }
}

fn require_name(rest: &str, usage: &str) -> Result<std::string::String, String> {
    let name = rest.trim();
    if name.is_empty() {
        return Err(std::format!("usage: .{usage}"));
    }
    Ok(name.to_string())
}

fn parse_ordinal(rest: &str, usage: &str) -> Result<i64, String> {
    let t = rest.trim();
    if t.is_empty() {
        return Err(std::format!("usage: .{usage}"));
    }
    t.parse()
        .map_err(|_| std::format!("invalid entry ordinal '{t}'"))
}

fn plain(text: std::string::String) -> InvokeResult {
    InvokeResult {
        text,
        state_deltas: std::vec![],
    }
}

fn truncate(s: &str, max_bytes: usize) -> std::string::String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Truncate on a char boundary <= max_bytes to avoid slicing mid-codepoint.
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    std::format!("{}\u{2026}", &s[..cut])
}

/// Plain ASCII bordered table, matching the standalone `fieldbook` CLI's
/// look-and-feel. Cells are rendered as-is; newlines are flattened to spaces
/// so multi-line SQL entries stay one row wide.
fn text_table(
    headers: &[&str],
    rows: &[std::vec::Vec<std::string::String>],
) -> std::string::String {
    let ncols = headers.len();
    let mut widths: std::vec::Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                let w = display_width(cell);
                if w > widths[i] {
                    widths[i] = w;
                }
            }
        }
    }
    let mut out = std::string::String::new();
    write_border(&mut out, &widths);
    write_row(
        &mut out,
        &headers.iter().map(|s| s.to_string()).collect::<std::vec::Vec<_>>(),
        &widths,
    );
    write_border(&mut out, &widths);
    for row in rows {
        write_row(&mut out, row, &widths);
    }
    write_border(&mut out, &widths);
    out
}

fn write_border(out: &mut std::string::String, widths: &[usize]) {
    out.push('+');
    for w in widths {
        for _ in 0..(w + 2) {
            out.push('-');
        }
        out.push('+');
    }
    out.push('\n');
}

fn write_row(out: &mut std::string::String, row: &[std::string::String], widths: &[usize]) {
    out.push('|');
    for (i, w) in widths.iter().enumerate() {
        let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
        // Replace newlines with spaces so multi-line SQL entries stay one row.
        let flat = cell.replace('\n', " ");
        let pad = w.saturating_sub(display_width(&flat));
        out.push(' ');
        out.push_str(&flat);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push(' ');
        out.push('|');
    }
    out.push('\n');
}

/// Approximate display width — one column per char. Good enough for the ASCII
/// / mostly-ASCII text this component emits.
fn display_width(s: &str) -> usize {
    s.chars().count()
}
