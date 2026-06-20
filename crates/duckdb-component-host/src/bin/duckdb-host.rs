use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{ArgAction, Parser};
use duckdb_component_host::{
    precompile_component_to_file, run_cli_with_stdio, serve_ui, set_extension_root,
    ComponentArtifacts,
};

#[derive(Parser, Debug)]
#[command(
    name = "duckdb-host",
    about = "Host runner for the DuckDB WebAssembly CLI component",
    trailing_var_arg = true,
    arg_required_else_help = true
)]
struct Opts {
    /// Override path to the compiled duckdb_core_component.wasm artifact
    #[arg(long)]
    core_component: Option<PathBuf>,

    /// Override path to the compiled duckdb_cli_component.wasm artifact
    #[arg(long)]
    cli_component: Option<PathBuf>,

    /// Directory that contains componentized extensions (defaults to artifacts/extensions)
    #[arg(long, default_value = "artifacts/extensions")]
    extensions_dir: PathBuf,

    /// Add a directory preopen mapping (HOST::GUEST) available to the CLI component
    #[arg(long = "dir", value_parser = parse_dir_mapping, action = ArgAction::Append)]
    preopen: Vec<DirMapping>,

    /// Arguments forwarded to the DuckDB CLI component (prefix with `--`)
    cli_args: Vec<String>,
}

#[derive(Debug, Clone)]
struct DirMapping {
    host: PathBuf,
    guest: String,
}

fn parse_dir_mapping(value: &str) -> Result<DirMapping, String> {
    let (host_str, guest_str) = value
        .split_once("::")
        .ok_or_else(|| "expected --dir HOST::GUEST".to_string())?;
    if guest_str.is_empty() {
        return Err("guest directory name cannot be empty".to_string());
    }
    if host_str.is_empty() {
        return Err("host directory path cannot be empty".to_string());
    }
    Ok(DirMapping {
        host: PathBuf::from(host_str),
        guest: guest_str.to_string(),
    })
}

fn main() -> Result<()> {
    // `duckdb-host precompile <in.wasm> <out.cwasm>` AOT-compiles a component so
    // a later run loads it (by passing the .cwasm path) without the Cranelift
    // compile. Handled before clap, which uses trailing_var_arg and would
    // otherwise treat these as CLI arguments.
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("precompile") {
        match (raw.get(2), raw.get(3)) {
            (Some(input), Some(output)) => {
                precompile_component_to_file(Path::new(input), Path::new(output))?;
                eprintln!("precompiled {input} -> {output}");
                return Ok(());
            }
            _ => {
                eprintln!("usage: duckdb-host precompile <in.wasm> <out.cwasm>");
                std::process::exit(2);
            }
        }
    }

    // `duckdb-host ui [--port N] [--no-open] [DB]` starts the local web SQL
    // console (our wasm-friendly equivalent of DuckDB's `ui` extension). The host
    // owns the listening socket and bridges queries to the core component.
    if raw.get(1).map(String::as_str) == Some("ui") {
        let mut port: u16 = 4213;
        let mut open_browser = true;
        let mut db: Option<String> = None;
        let mut i = 2;
        while i < raw.len() {
            match raw[i].as_str() {
                "--port" => {
                    i += 1;
                    port = raw.get(i).and_then(|s| s.parse().ok()).unwrap_or(port);
                }
                "--no-open" => open_browser = false,
                other => db = Some(other.to_string()),
            }
            i += 1;
        }

        let artifacts = ComponentArtifacts::resolve_default()?;
        let extensions_dir = std::env::current_dir()?.join("artifacts/extensions");
        set_extension_root(extensions_dir);
        let cwd = std::env::current_dir()?;
        let preopens: Vec<(&Path, &str)> = vec![(cwd.as_path(), ".")];
        serve_ui(&artifacts, db.as_deref(), port, open_browser, &preopens)?;
        return Ok(());
    }

    let opts = Opts::parse();

    if opts.cli_args.is_empty() {
        anyhow::bail!("pass CLI arguments after `--`, e.g. duckdb-host -- duckdb-cli :memory:");
    }

    let artifacts = match (opts.core_component, opts.cli_component) {
        (Some(core), Some(cli)) => ComponentArtifacts::new(core, cli),
        (None, None) => ComponentArtifacts::resolve_default()?,
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!(
                "specify both --core-component and --cli-component when overriding artifacts"
            )
        }
    };

    let extensions_dir = if opts.extensions_dir.is_absolute() {
        opts.extensions_dir.clone()
    } else {
        std::env::current_dir()?.join(opts.extensions_dir)
    };
    set_extension_root(extensions_dir);

    let mut mappings = Vec::new();
    mappings.push(DirMapping {
        host: std::env::current_dir()?,
        guest: ".".into(),
    });
    mappings.extend(opts.preopen);

    let preopen_refs: Vec<(&Path, &str)> = mappings
        .iter()
        .map(|entry| (entry.host.as_path(), entry.guest.as_str()))
        .collect();

    let status = run_cli_with_stdio(&artifacts, &opts.cli_args, &preopen_refs)?;
    if status.is_err() {
        std::process::exit(1);
    }
    Ok(())
}
