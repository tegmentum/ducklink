use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{ArgAction, Parser};
use duckdb_component_host::{run_cli_with_stdio, set_extension_root, ComponentArtifacts};

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
