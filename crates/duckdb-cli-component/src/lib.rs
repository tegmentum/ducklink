use std::fmt::Write as _;

mod bindings;

use bindings::duckdb::component::database as duckdb;
use bindings::wasi::cli::environment;
use bindings::wasi::cli::stderr;
use bindings::wasi::cli::stdin;
use bindings::wasi::cli::stdout;
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

    if let Some(sql) = command {
        execute_and_print(&connection, &sql)?;
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

fn run_repl(conn: &duckdb::Connection) -> Result<(), String> {
    let input = stdin::get_stdin();
    let out = stdout::get_stdout();
    let mut buffer = Vec::new();
    let mut statement = String::new();

    write_prompt(&out)?;
    while let Some(line) = read_line(&input, &mut buffer)? {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case(".quit") || trimmed.eq_ignore_ascii_case(".exit") {
            break;
        }
        if trimmed.eq_ignore_ascii_case(".help") {
            print_help();
            statement.clear();
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
            execute_and_print(conn, &statement)?;
        }
        statement.clear();
        write_prompt(&out)?;
    }

    Ok(())
}

fn render_result(result: duckdb::QueryResult) -> Result<(), String> {
    let out = stdout::get_stdout();

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
    write_line(&out, &line)?;
    line.clear();
    format_separator(&mut line, &widths);
    write_line(&out, &line)?;

    for row in rendered_rows {
        line.clear();
        format_data_row(&mut line, &row, &widths);
        write_line(&out, &line)?;
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
                let line = flush_buffer(buffer)?;
                return Ok(Some(line));
            }
            Err(streams::StreamError::LastOperationFailed(err)) => {
                return Err(format!("read failed: {err:?}"));
            }
        }

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
    let _ = write_line(&out, "Interactive commands: .help, .exit, .quit");
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
    }
}
