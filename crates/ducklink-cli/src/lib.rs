use std::fmt::Write as _;

mod bindings;

use bindings::duckdb::component::database as duckdb;
use bindings::wasi::cli::environment;
use bindings::wasi::cli::stderr;
use bindings::wasi::cli::stdin;
use bindings::wasi::cli::stdout;
use bindings::wasi::filesystem::preopens;
use bindings::wasi::filesystem::types as fs_types;
use bindings::wasi::io::streams;

struct Component;

impl bindings::exports::wasi::cli::run::Guest for Component {
    fn run() -> Result<(), ()> {
        if let Err(err) = run_cli() {
            emit_error(&err).ok();
            Err(())
        } else {
            Ok(())
        }
    }
}

bindings::exports::wasi::cli::run::__export_wasi_cli_run_0_2_6_cabi!(
    Component with_types_in bindings::exports::wasi::cli::run
);

fn run_cli() -> Result<(), String> {
    let args = environment::get_arguments();
    let mut positional = Vec::new();
    let mut command: Option<String> = None;
    let mut preload_extensions = Vec::new();

    let mut iter = args.into_iter().skip(1);
    let mut passthrough = false;
    while let Some(arg) = iter.next() {
        if !passthrough {
            match arg.as_str() {
                "--" => {
                    passthrough = true;
                    continue;
                }
                "--dir" | "--mapdir" | "--dir::" | "--dir:ro" | "--mapdir::" | "--mapdir:ro" => {
                    let _ = iter.next();
                    continue;
                }
                "--env" | "--env-file" => {
                    let _ = iter.next();
                    continue;
                }
                _ => {}
            }
        }

        match arg.as_str() {
            "-c" | "--command" => {
                if command.is_some() {
                    return Err("duplicate -c/--command option".into());
                }
                let value = iter
                    .next()
                    .ok_or_else(|| "expected SQL after -c/--command".to_string())?;
                command = Some(value);
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "--load-extension" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "expected extension name after --load-extension".to_string())?;
                preload_extensions.push(value);
            }
            other => {
                positional.push(other.to_string());
            }
        }
    }

    let database_path = positional.first().cloned();
    let connection = open_connection(database_path.clone())?;
    load_extensions(&connection, &preload_extensions)?;
    // Auto-load extensions a previous session persisted in this database.
    autoload_persisted_extensions(&connection);

    if let Some(sql) = command {
        dispatch_statement(&connection, &sql)?;
        duckdb::close(connection);
        return Ok(());
    }

    if positional.len() > 1 {
        return Err("unexpected additional positional arguments".into());
    }

    run_repl(&connection)?;
    duckdb::close(connection);
    Ok(())
}

fn open_connection(path: Option<String>) -> Result<duckdb::Connection, String> {
    duckdb::open(path.as_ref().map(|s| s.as_str()))
        .map_err(|err| format!("failed to open database: {err}"))
}

fn load_extensions(conn: &duckdb::Connection, extensions: &[String]) -> Result<(), String> {
    for ext in extensions {
        let sql = format!("LOAD {};", ext);
        duckdb::execute(conn, &sql)
            .map(|_| ())
            .map_err(duckerror_to_string)?;
    }
    Ok(())
}

fn execute_and_print(conn: &duckdb::Connection, sql: &str) -> Result<(), String> {
    let result = duckdb::execute(conn, sql).map_err(duckerror_to_string)?;
    render_result(result)
}

/// Run one complete input line: a `.`-prefixed meta-command, or SQL. `.quit`/
/// `.exit` are loop control handled by the REPL and are no-ops here (e.g. via
/// `-c`).
fn dispatch_statement(conn: &duckdb::Connection, statement: &str) -> Result<(), String> {
    let trimmed = statement.trim();
    if let Some(rest) = trimmed.strip_prefix('.') {
        return run_meta_command(conn, rest);
    }
    let result = execute_and_print(conn, statement);
    // Remember each successful `LOAD <ext>` so a future session on this db
    // auto-loads it. `-c` can pass several `;`-separated statements at once.
    if result.is_ok() {
        for name in parse_load_names(trimmed) {
            persist_loaded_extension(conn, &name);
        }
    }
    result
}

const PERSIST_TABLE: &str = "__ducklink_loaded_extensions";

/// Extract the extension name from every `LOAD <name>` among the `;`-separated
/// statements in `input` (quotes stripped). Splitting on `;` is a heuristic, but
/// extension names are bare identifiers and a stray match merely re-LOADs.
fn parse_load_names(input: &str) -> Vec<String> {
    input
        .split(';')
        .filter_map(|stmt| {
            let mut it = stmt.trim().splitn(2, char::is_whitespace);
            if !it.next()?.eq_ignore_ascii_case("load") {
                return None;
            }
            let name = it.next()?.trim().trim_matches(|c| c == '\'' || c == '"').trim();
            (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string())
        })
        .collect()
}

/// Record a loaded extension in the db so a future session on the same database
/// auto-loads it. Best-effort (errors ignored; ephemeral for `:memory:`).
fn persist_loaded_extension(conn: &duckdb::Connection, name: &str) {
    let _ = duckdb::execute(
        conn,
        &format!("CREATE TABLE IF NOT EXISTS {PERSIST_TABLE}(name VARCHAR PRIMARY KEY)"),
    );
    let _ = duckdb::execute(
        conn,
        &format!(
            "INSERT INTO {PERSIST_TABLE} VALUES ('{}') ON CONFLICT DO NOTHING",
            escape_sql_literal(name)
        ),
    );
}

/// On startup, LOAD every extension a previous session persisted in this db.
fn autoload_persisted_extensions(conn: &duckdb::Connection) {
    let result = match duckdb::execute(conn, &format!("SELECT name FROM {PERSIST_TABLE}")) {
        Ok(r) => r,
        Err(_) => return, // table absent (fresh db) -> nothing to auto-load
    };
    for row in result.rows {
        if let Some(duckdb::Duckvalue::Text(name)) = row.into_iter().next() {
            // Direct execute (not dispatch_statement) so this doesn't re-persist.
            let _ = duckdb::execute(conn, &format!("LOAD {name}"));
        }
    }
}

/// Translate a dot meta-command into the SQL that backs it, mirroring a subset
/// of the native DuckDB/SQLite shell. The optional argument restricts the
/// listing to a single table (a `LIKE` pattern for `.tables`).
fn run_meta_command(conn: &duckdb::Connection, rest: &str) -> Result<(), String> {
    let mut parts = rest.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next().map(|s| s.trim_end_matches(';').to_string());
    match cmd.as_str() {
        "help" => {
            print_help();
            Ok(())
        }
        // Loop control; handled by the REPL, no-op elsewhere.
        "quit" | "exit" => Ok(()),
        // .tables / .schema / .indexes are now provided by the pluggable
        // `core-dotcmd` component (extensions/core-dotcmd) — they fall through
        // to the dot-command registry below instead of being hard-coded here.
        "read" => {
            // DuckDB reads the file (via the core fs shims); run its contents as
            // a script. Errors abort the script.
            let path = arg.ok_or_else(|| ".read requires a file path".to_string())?;
            let sql = format!(
                "SELECT content FROM read_text('{}')",
                escape_sql_literal(&path)
            );
            let result = duckdb::execute(conn, &sql).map_err(duckerror_to_string)?;
            let content = match result
                .rows
                .into_iter()
                .next()
                .and_then(|mut r| (!r.is_empty()).then(|| r.remove(0)))
            {
                Some(duckdb::Duckvalue::Text(text)) => text,
                Some(other) => format_duckvalue(other),
                None => return Err(format!("could not read '{path}'")),
            };
            run_script(conn, &content)
        }
        "import" => {
            // `.import FILE TABLE` bulk-loads a CSV into an existing table.
            let file = arg.ok_or_else(|| ".import requires FILE and TABLE".to_string())?;
            let table = parts
                .next()
                .map(|t| t.trim_end_matches(';').to_string())
                .ok_or_else(|| ".import requires FILE and TABLE".to_string())?;
            let sql = format!(
                "COPY \"{}\" FROM '{}' (AUTO_DETECT true)",
                table.replace('"', "\"\""),
                escape_sql_literal(&file)
            );
            duckdb::execute(conn, &sql)
                .map(|_| ())
                .map_err(duckerror_to_string)
        }
        "mode" => {
            let mode = arg
                .ok_or_else(|| ".mode requires a format (table|csv|json)".to_string())?;
            let selected = match mode.to_ascii_lowercase().as_str() {
                "table" | "box" | "column" => OutputMode::Table,
                "csv" => OutputMode::Csv,
                "json" => OutputMode::Json,
                other => {
                    return Err(format!("unknown output mode '{other}' (table|csv|json)"))
                }
            };
            OUTPUT_MODE.with(|m| m.set(selected));
            Ok(())
        }
        "output" => {
            // `.output FILE` redirects subsequent output to a file;
            // `.output` / `.output stdout` restores stdout.
            match arg.as_deref() {
                None | Some("stdout") | Some("-") => {
                    OUTPUT_FILE.with(|c| *c.borrow_mut() = None);
                    Ok(())
                }
                Some(path) => {
                    let file = open_output_file(path)?;
                    OUTPUT_FILE.with(|c| *c.borrow_mut() = Some(file));
                    Ok(())
                }
            }
        }
        other => {
            // Unknown built-in: fall through to a pluggable dot-command
            // component (host-mediated). `args` is everything after the name.
            let args = rest
                .trim()
                .splitn(2, char::is_whitespace)
                .nth(1)
                .unwrap_or("")
                .trim();
            match bindings::duckdb::cli::dotcmd_host::invoke(other, args) {
                Ok(Some(outcome)) => {
                    for delta in &outcome.state_deltas {
                        apply_state_delta(&delta.key, &delta.value);
                    }
                    if outcome.text.is_empty() {
                        Ok(())
                    } else {
                        write_all(&stdout::get_stdout(), outcome.text.as_bytes())
                    }
                }
                Ok(None) => Err(format!("unknown command: .{other} (try .help)")),
                Err(message) => Err(message),
            }
        }
    }
}

/// Apply a session-state delta emitted by a dot-command component. Slash-
/// namespaced keys; unknown keys/values are silently ignored (forward-compatible).
fn apply_state_delta(key: &str, value: &str) {
    if key == "display/mode" {
        let mode = match value.to_ascii_lowercase().as_str() {
            "csv" => OutputMode::Csv,
            "json" => OutputMode::Json,
            "table" | "box" | "column" => OutputMode::Table,
            _ => return,
        };
        OUTPUT_MODE.with(|m| m.set(mode));
    }
}

/// Escape single quotes for safe interpolation into a SQL string literal.
fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn run_repl(conn: &duckdb::Connection) -> Result<(), String> {
    let input = stdin::get_stdin();
    let out = stdout::get_stdout();
    let mut buffer = Vec::new();
    let mut statement = String::new();

    write_prompt(&out)?;
    while let Some(line) = read_line(&input, &mut buffer)? {
        let trimmed = line.trim();
        // Meta-commands are single-line and only recognized at the start of a
        // statement (so a `.` inside multi-line SQL is left untouched).
        if statement.trim().is_empty() && trimmed.starts_with('.') {
            if trimmed.eq_ignore_ascii_case(".quit") || trimmed.eq_ignore_ascii_case(".exit") {
                break;
            }
            // Errors in the REPL are reported but do not end the session.
            if let Err(err) = dispatch_statement(conn, trimmed) {
                emit_error(&err).ok();
            }
            write_prompt(&out)?;
            continue;
        }

        statement.push_str(trimmed);
        if !trimmed.ends_with(';') {
            statement.push(' ');
            write_continuation_prompt(&out)?;
            continue;
        }

        if !statement.trim().is_empty() {
            if let Err(err) = dispatch_statement(conn, &statement) {
                emit_error(&err).ok();
            }
        }
        statement.clear();
        write_prompt(&out)?;
    }

    Ok(())
}

/// Output format selected by `.mode`. Defaults to the box-style table.
#[derive(Clone, Copy, PartialEq)]
enum OutputMode {
    Table,
    Csv,
    Json,
}

thread_local! {
    static OUTPUT_MODE: std::cell::Cell<OutputMode> = std::cell::Cell::new(OutputMode::Table);
}

/// A file opened by `.output`. The descriptor is held alongside the write stream
/// so the file stays open for the stream's lifetime; dropping it flushes/closes.
struct OutputFile {
    _descriptor: fs_types::Descriptor,
    stream: streams::OutputStream,
}

thread_local! {
    static OUTPUT_FILE: std::cell::RefCell<Option<OutputFile>> =
        const { std::cell::RefCell::new(None) };
}

fn render_result(result: duckdb::QueryResult) -> Result<(), String> {
    OUTPUT_FILE.with(|cell| {
        let target = cell.borrow();
        match target.as_ref() {
            Some(file) => render_to(result, &file.stream),
            None => render_to(result, &stdout::get_stdout()),
        }
    })
}

/// Opens `path` (truncating) for `.output`, resolving it against the WASI
/// preopened directories and returning the file descriptor + its write stream.
fn open_output_file(path: &str) -> Result<OutputFile, String> {
    let preopens = preopens::get_directories();
    let (descriptor, relative) = resolve_output_path(path, &preopens)?;
    let file = descriptor
        .open_at(
            fs_types::PathFlags::empty(),
            &relative,
            fs_types::OpenFlags::CREATE | fs_types::OpenFlags::TRUNCATE,
            fs_types::DescriptorFlags::WRITE,
        )
        .map_err(|err| format!("cannot open '{path}' for output: {err:?}"))?;
    let stream = file
        .write_via_stream(0)
        .map_err(|err| format!("cannot write '{path}': {err:?}"))?;
    Ok(OutputFile {
        _descriptor: file,
        stream,
    })
}

/// Matches `path` against a preopened directory, returning that directory's
/// descriptor and the sub-path relative to it. Relative paths land in the cwd
/// preopen (`.`); absolute paths are matched by prefix; otherwise the first
/// preopen is used.
fn resolve_output_path<'a>(
    path: &str,
    preopens: &'a [(fs_types::Descriptor, String)],
) -> Result<(&'a fs_types::Descriptor, String), String> {
    let relative = path.trim_start_matches("./");
    for (descriptor, root) in preopens {
        let root = root.trim_end_matches('/');
        if (root.is_empty() || root == ".") && !path.starts_with('/') {
            return Ok((descriptor, relative.to_string()));
        }
        if let Some(rest) = path.strip_prefix(root) {
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                return Ok((descriptor, rest.to_string()));
            }
        }
    }
    preopens
        .first()
        .map(|(descriptor, _)| (descriptor, relative.to_string()))
        .ok_or_else(|| "no writable directory available (run with --dir)".to_string())
}

fn render_to(result: duckdb::QueryResult, out: &streams::OutputStream) -> Result<(), String> {
    match OUTPUT_MODE.with(|m| m.get()) {
        OutputMode::Table => render_table(result, out),
        OutputMode::Csv => render_csv(result, out),
        OutputMode::Json => render_json(result, out),
    }
}

fn render_table(result: duckdb::QueryResult, out: &streams::OutputStream) -> Result<(), String> {
    let duckdb::QueryResult { columns, rows } = result;
    let mut widths: Vec<usize> = columns.iter().map(|c| c.name.len()).collect();
    let mut rendered_rows = Vec::with_capacity(rows.len());

    for row in rows {
        let mut rendered = Vec::with_capacity(row.len());
        for (idx, value) in row.into_iter().enumerate() {
            let formatted = format_duckvalue(value);
            if let Some(width) = widths.get_mut(idx) {
                if formatted.len() > *width {
                    *width = formatted.len();
                }
            } else {
                widths.push(formatted.len());
            }
            rendered.push(formatted);
        }
        rendered_rows.push(rendered);
    }

    let mut line = String::new();
    format_header_row(&mut line, &columns, &widths);
    write_line(out, &line)?;
    line.clear();
    format_separator(&mut line, &widths);
    write_line(out, &line)?;

    for row in rendered_rows {
        line.clear();
        format_data_row(&mut line, &row, &widths);
        write_line(out, &line)?;
    }

    Ok(())
}

fn format_header_row(line: &mut String, columns: &[duckdb::Columndef], widths: &[usize]) {
    line.push('|');
    for (idx, column) in columns.iter().enumerate() {
        let value = column.name.as_str();
        let width = widths.get(idx).copied().unwrap_or(value.len());
        let _ = write!(line, " {:width$} |", value, width = width);
    }
}

fn format_data_row(line: &mut String, cells: &[String], widths: &[usize]) {
    line.push('|');
    for (idx, &width) in widths.iter().enumerate() {
        let value = cells.get(idx).map(|s| s.as_str()).unwrap_or("");
        let _ = write!(line, " {:width$} |", value, width = width);
    }
}

fn format_separator(line: &mut String, widths: &[usize]) {
    line.push('+');
    for width in widths {
        line.push_str(&"-".repeat(width + 2));
        line.push('+');
    }
}

/// `blocking-write-and-flush` accepts at most 4096 bytes per the WASI spec, so
/// split larger payloads into chunks before handing them to the stream.
fn write_all(stream: &streams::OutputStream, bytes: &[u8]) -> Result<(), String> {
    for chunk in bytes.chunks(4096) {
        stream
            .blocking_write_and_flush(chunk)
            .map_err(|err| format!("write failed: {err:?}"))?;
    }
    Ok(())
}

fn write_line(stream: &streams::OutputStream, text: &str) -> Result<(), String> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\n');
    write_all(stream, &bytes)
}

fn write_prompt(stream: &streams::OutputStream) -> Result<(), String> {
    stream
        .blocking_write_and_flush(b"D> ")
        .map_err(|err| format!("write failed: {err:?}"))
}

fn write_continuation_prompt(stream: &streams::OutputStream) -> Result<(), String> {
    stream
        .blocking_write_and_flush(b"...> ")
        .map_err(|err| format!("write failed: {err:?}"))
}

fn read_line(
    stream: &streams::InputStream,
    buffer: &mut Vec<u8>,
) -> Result<Option<String>, String> {
    loop {
        // Serve any complete line already sitting in the buffer before reading
        // more. A single `blocking_read` can return many lines at once (piped
        // input arrives in 1024-byte chunks); without draining the buffer first
        // we would block-read again, hit `Closed` at EOF with the remaining
        // lines still buffered, and collapse them into one mega-statement.
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut drained = buffer.drain(..=position).collect::<Vec<_>>();
            if let Some(last) = drained.last() {
                if *last == b'\n' {
                    drained.pop();
                }
            }
            let line =
                String::from_utf8(drained).map_err(|_| "stdin is not valid UTF-8".to_string())?;
            return Ok(Some(line));
        }

        match stream.blocking_read(1024) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                buffer.extend_from_slice(&chunk);
            }
            Err(streams::StreamError::Closed) => {
                if buffer.is_empty() {
                    return Ok(None);
                }
                // EOF with a trailing line that had no newline terminator.
                let line = flush_buffer(buffer)?;
                return Ok(Some(line));
            }
            Err(streams::StreamError::LastOperationFailed(err)) => {
                return Err(format!("read failed: {err:?}"));
            }
        }
    }
}

fn flush_buffer(buffer: &mut Vec<u8>) -> Result<String, String> {
    if buffer.is_empty() {
        return Ok(String::new());
    }
    let data = std::mem::take(buffer);
    String::from_utf8(data).map_err(|_| "input is not valid UTF-8".to_string())
}

fn emit_error(message: &str) -> Result<(), String> {
    let stream = stderr::get_stderr();
    let mut bytes = message.as_bytes().to_vec();
    bytes.push(b'\n');
    write_all(&stream, &bytes).map_err(|err| format!("stderr {err}"))
}

fn print_help() {
    let out = stdout::get_stdout();
    let _ = write_line(
        &out,
        "DuckDB WASI CLI\n\nUsage: duckdb [database] [-c SQL] [script]\n",
    );
    let _ = write_line(
        &out,
        "Options:\n  -c, --command SQL        Execute SQL and exit\n  --load-extension NAME   Preload a component extension before running SQL",
    );
    let _ = write_line(
        &out,
        "Built-in meta-commands:\n  \
         .import FILE TABLE  Load a CSV file into an existing table\n  \
         .read FILE          Execute SQL statements from a file\n  \
         .mode FORMAT        Set output format: table, csv, or json\n  \
         .output [FILE]      Redirect output to FILE (no arg / stdout resets)\n  \
         .help               Show this help\n  \
         .exit, .quit        Leave the shell",
    );
    // Pluggable dot commands provided by loaded dot-command components
    // (.tables / .schema / etc. now live in core-dotcmd).
    let commands = bindings::duckdb::cli::dotcmd_host::list_commands();
    if !commands.is_empty() {
        let _ = write_line(&out, "\nPlugin dot commands:");
        for c in &commands {
            let _ = write_line(&out, &format!("  .{:<18}{}", c.usage, c.summary));
        }
    }
}

fn duckerror_to_string(err: duckdb::Duckerror) -> String {
    match err {
        duckdb::Duckerror::Invalidargument(msg) => format!("invalid argument: {msg}"),
        duckdb::Duckerror::Unsupported(msg) => format!("unsupported: {msg}"),
        duckdb::Duckerror::Invalidstate(msg) => format!("invalid state: {msg}"),
        duckdb::Duckerror::Io(msg) => format!("io error: {msg}"),
        duckdb::Duckerror::Internal(msg) => format!("internal error: {msg}"),
    }
}

fn render_csv(result: duckdb::QueryResult, out: &streams::OutputStream) -> Result<(), String> {
    let duckdb::QueryResult { columns, rows } = result;
    let header: Vec<String> = columns.iter().map(|c| csv_field(&c.name)).collect();
    write_line(out, &header.join(","))?;
    for row in rows {
        let cells: Vec<String> = row
            .into_iter()
            .map(|v| csv_field(&format_duckvalue(v)))
            .collect();
        write_line(out, &cells.join(","))?;
    }
    Ok(())
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn render_json(result: duckdb::QueryResult, out: &streams::OutputStream) -> Result<(), String> {
    let duckdb::QueryResult { columns, rows } = result;
    let mut json = String::from("[");
    for (ri, row) in rows.into_iter().enumerate() {
        if ri > 0 {
            json.push(',');
        }
        json.push('{');
        for (ci, value) in row.into_iter().enumerate() {
            if ci > 0 {
                json.push(',');
            }
            let name = columns.get(ci).map(|c| c.name.as_str()).unwrap_or("");
            json.push_str(&json_string(name));
            json.push(':');
            json.push_str(&json_value(value));
        }
        json.push('}');
    }
    json.push(']');
    write_line(out, &json)
}

fn json_value(value: duckdb::Duckvalue) -> String {
    match value {
        duckdb::Duckvalue::Null => "null".to_string(),
        duckdb::Duckvalue::Boolean(b) => b.to_string(),
        duckdb::Duckvalue::Int64(v) => v.to_string(),
        duckdb::Duckvalue::Uint64(v) => v.to_string(),
        duckdb::Duckvalue::Float64(v) if v.is_finite() => v.to_string(),
        // Text, Blob, and non-finite floats render as JSON strings.
        other => json_string(&format_duckvalue(other)),
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Execute a multi-statement SQL/meta-command script (used by `.read`),
/// splitting on `;` like the REPL and dispatching each statement. Errors abort
/// the script.
fn run_script(conn: &duckdb::Connection, script: &str) -> Result<(), String> {
    let mut statement = String::new();
    for line in script.lines() {
        let trimmed = line.trim();
        if statement.trim().is_empty() && trimmed.starts_with('.') {
            dispatch_statement(conn, trimmed)?;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !statement.is_empty() {
            statement.push(' ');
        }
        statement.push_str(trimmed);
        if trimmed.ends_with(';') {
            dispatch_statement(conn, &statement)?;
            statement.clear();
        }
    }
    if !statement.trim().is_empty() {
        dispatch_statement(conn, &statement)?;
    }
    Ok(())
}

fn format_duckvalue(value: duckdb::Duckvalue) -> String {
    match value {
        duckdb::Duckvalue::Null => "NULL".to_string(),
        duckdb::Duckvalue::Boolean(true) => "true".to_string(),
        duckdb::Duckvalue::Boolean(false) => "false".to_string(),
        duckdb::Duckvalue::Int64(value) => value.to_string(),
        duckdb::Duckvalue::Uint64(value) => value.to_string(),
        duckdb::Duckvalue::Float64(value) => value.to_string(),
        duckdb::Duckvalue::Text(text) => text,
        duckdb::Duckvalue::Blob(bytes) => {
            let mut out = String::with_capacity(bytes.len().saturating_mul(2) + 2);
            out.push_str("0x");
            for byte in bytes {
                let _ = write!(out, "{byte:02x}");
            }
            out
        }
        duckdb::Duckvalue::Int32(value) => value.to_string(),
        duckdb::Duckvalue::Timestamp(micros) => format_timestamp_micros(micros),
        duckdb::Duckvalue::Int8(value) => value.to_string(),
        duckdb::Duckvalue::Int16(value) => value.to_string(),
        duckdb::Duckvalue::Uint8(value) => value.to_string(),
        duckdb::Duckvalue::Uint16(value) => value.to_string(),
        duckdb::Duckvalue::Uint32(value) => value.to_string(),
        duckdb::Duckvalue::Float32(value) => value.to_string(),
        duckdb::Duckvalue::Date(days) => format_date_days(days),
        duckdb::Duckvalue::Time(micros) => format_time_micros(micros),
        // TIMESTAMP_TZ renders as the UTC wall-clock plus a +00 zone suffix
        // (the session default), matching DuckDB's own VARCHAR cast.
        duckdb::Duckvalue::Timestamptz(micros) => {
            format!("{}+00", format_timestamp_micros(micros))
        }
    }
}

/// Render a DuckDB DATE (days since 1970-01-01) as `YYYY-MM-DD`.
fn format_date_days(days: i32) -> String {
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

/// Render a DuckDB TIME (microseconds since midnight) as `HH:MM:SS[.ffffff]`.
fn format_time_micros(micros: i64) -> String {
    let (secs, frac_us) = (micros.div_euclid(1_000_000), micros.rem_euclid(1_000_000));
    let (hh, mm, ss) = (secs / 3_600, (secs % 3_600) / 60, secs % 60);
    if frac_us == 0 {
        format!("{hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{hh:02}:{mm:02}:{ss:02}.{frac_us:06}")
    }
}

/// Render a DuckDB TIMESTAMP (microseconds since 1970-01-01 UTC) as
/// `YYYY-MM-DD HH:MM:SS[.ffffff]`, matching DuckDB's own VARCHAR cast.
fn format_timestamp_micros(micros: i64) -> String {
    let (secs, frac_us) = (micros.div_euclid(1_000_000), micros.rem_euclid(1_000_000));
    // Civil-from-days (Howard Hinnant's algorithm).
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let (hh, mm, ss) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);
    if frac_us == 0 {
        format!("{year:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{year:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{frac_us:06}")
    }
}
