use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{ArgAction, Parser};
use duckdb_component_host::{
    precompile_component_to_file, run_cli_with_stdio, serve_httpd, serve_ui, set_extension_root,
    ComponentArtifacts, HandlerRegistry, HttpdOptions, TlsMode, UiMode,
};

#[derive(Parser, Debug)]
#[command(
    name = "ducklink",
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

    /// Grant outbound network to extension components: "all" / "*" for every
    /// extension, or a comma-separated allowlist of names (e.g. "dns,http").
    /// Default: denied. Convenience for the DUCKLINK_NETWORK_GRANT env var
    /// (this flag wins when both are set).
    #[arg(long = "grant-network")]
    grant_network: Option<String>,

    /// Arguments forwarded to the DuckDB CLI component (prefix with `--`)
    cli_args: Vec<String>,
}

#[derive(Debug, Clone)]
struct DirMapping {
    host: PathBuf,
    guest: String,
}

/// `compose` — build a core with selected extensions embedded at compile time,
/// the command-line counterpart of sqlite-wasm's `sqlink compose`. `--embed
/// a,b,c` may mix two kinds, discovered + dispatched automatically:
///   - Rust component extensions (core crate `embed-<name>` features) -> a fast
///     `cargo component build --features embed-<name>` (no libduckdb rebuild).
///   - C++ official extensions (cmake/wasm-extension-config.cmake) -> a libduckdb
///     rebuild with EMBED_EXTENSIONS=<those> (slow; static C++ link), then relink.
/// The default core embeds nothing (fully lean); everything else loads at runtime.
fn run_compose(args: &[String]) -> Result<()> {
    let mut list = false;
    let mut embed: Vec<String> = Vec::new();
    let mut output: Option<PathBuf> = None;
    let mut precompile_after = false;
    let mut repo_root = std::env::current_dir()?;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--embed" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--embed expects a comma-separated list"))?;
                embed = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--output" => {
                i += 1;
                output = Some(PathBuf::from(
                    args.get(i).ok_or_else(|| anyhow::anyhow!("--output expects a path"))?,
                ));
            }
            "--precompile" => precompile_after = true,
            "--repo-root" => {
                i += 1;
                repo_root = PathBuf::from(
                    args.get(i).ok_or_else(|| anyhow::anyhow!("--repo-root expects a path"))?,
                );
            }
            other => anyhow::bail!("compose: unknown arg {other:?}"),
        }
        i += 1;
    }

    let rust_exts = discover_embeddable_extensions(&repo_root)?;
    let cpp_exts = discover_cpp_extensions(&repo_root)?;
    if list {
        eprintln!("Rust component extensions (fast, cargo-feature embed):");
        if rust_exts.is_empty() {
            eprintln!("  (none expose an `embed-<name>` core feature)");
        } else {
            for n in &rust_exts {
                println!("{n}");
            }
        }
        eprintln!("C++ official extensions (libduckdb rebuild via EMBED_EXTENSIONS):");
        for n in &cpp_exts {
            println!("{n}");
        }
        return Ok(());
    }
    if embed.is_empty() {
        anyhow::bail!("compose: pass --embed NAME[,...] or --list");
    }

    // Partition the request into Rust (cargo feature) vs C++ (libduckdb) sets.
    let mut rust_sel: Vec<String> = Vec::new();
    let mut cpp_sel: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for e in &embed {
        if rust_exts.contains(e) {
            rust_sel.push(e.clone());
        } else if cpp_exts.contains(e) {
            cpp_sel.push(e.clone());
        } else {
            unknown.push(e.clone());
        }
    }
    if !unknown.is_empty() {
        anyhow::bail!(
            "compose: not embeddable: {}\n  Rust: {}\n  C++:  {}",
            unknown.join(", "),
            if rust_exts.is_empty() { "(none)".into() } else { rust_exts.join(", ") },
            cpp_exts.join(", ")
        );
    }

    // Step 1: rebuild libduckdb with the requested C++ extensions (if any). This
    // is the slow path (static C++ link); EMBED_EXTENSIONS drives the cmake config
    // + the build script's staging/patching/dep-merging.
    if !cpp_sel.is_empty() {
        for var in ["DUCKDB_SOURCE_DIR", "WASI_SDK_PREFIX"] {
            if std::env::var_os(var).is_none() {
                anyhow::bail!("compose: {var} must be set to rebuild libduckdb for C++ extensions");
            }
        }
        let script = repo_root.join("scripts/build-libduckdb-wasm.sh");
        eprintln!(
            "Rebuilding libduckdb with C++ extensions: {} (static link — this takes a while)",
            cpp_sel.join(", ")
        );
        let status = std::process::Command::new("bash")
            .arg(&script)
            .env("EMBED_EXTENSIONS", cpp_sel.join(","))
            .current_dir(&repo_root)
            .status()
            .map_err(|e| anyhow::anyhow!("spawn build-libduckdb-wasm.sh: {e}"))?;
        if !status.success() {
            anyhow::bail!("libduckdb rebuild failed");
        }
    }

    // Step 2: build the core (embedding the Rust extensions, relinking against the
    // libduckdb from step 1). Needs the archive env vars (like `make core`).
    for var in ["DUCKDB_STATIC_LIB", "DUCKDB_INCLUDE_DIR"] {
        if std::env::var_os(var).is_none() {
            anyhow::bail!("compose: {var} must be set (the prebuilt DuckDB archive / include dir)");
        }
    }

    // Map Rust names -> cargo features (hyphenated, as cargo requires on the CLI).
    let mut features = vec!["wasi".to_string()];
    features.extend(rust_sel.iter().map(|n| format!("embed-{}", n.replace('_', "-"))));
    let features = features.join(",");

    // Keep the generated WIT in sync, like the Makefile `core` target.
    let sync = repo_root.join("scripts/sync-core-wit.sh");
    if sync.is_file() {
        let _ = std::process::Command::new("bash").arg(&sync).current_dir(&repo_root).status();
    }

    if !rust_sel.is_empty() {
        eprintln!("Embedding Rust extensions: {}", rust_sel.join(", "));
    }
    eprintln!("  cargo component build -p duckdb-core-component --target wasm32-wasip2 --release --features {features}");
    let status = std::process::Command::new("cargo")
        .args([
            "component", "build", "-p", "duckdb-core-component", "--target", "wasm32-wasip2",
            "--release", "--features", &features,
        ])
        .current_dir(&repo_root)
        .status()
        .map_err(|e| anyhow::anyhow!("spawn cargo component: {e} (install with `cargo install cargo-component`)"))?;
    if !status.success() {
        anyhow::bail!("cargo component build failed");
    }

    let core_wasm = repo_root.join("target/wasm32-wasip2/release/duckdb_core_component.wasm");
    let final_wasm = if let Some(out) = &output {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&core_wasm, out)
            .map_err(|e| anyhow::anyhow!("copy {} -> {}: {e}", core_wasm.display(), out.display()))?;
        eprintln!("wrote {}", out.display());
        out.clone()
    } else {
        eprintln!("wrote {}", core_wasm.display());
        core_wasm
    };

    if precompile_after {
        let cwasm = final_wasm.with_extension("cwasm");
        precompile_component_to_file(&final_wasm, &cwasm)?;
        eprintln!("precompiled {} -> {}", final_wasm.display(), cwasm.display());
    }
    Ok(())
}

/// Discover embeddable extensions from the core crate's `[features]`: every
/// `embed-<name>` feature corresponds to an extension that can be compiled in.
fn discover_embeddable_extensions(repo_root: &Path) -> Result<Vec<String>> {
    let cargo_toml = repo_root.join("crates/duckdb-core-component/Cargo.toml");
    let text = std::fs::read_to_string(&cargo_toml)
        .map_err(|e| anyhow::anyhow!("read {}: {e} (pass --repo-root)", cargo_toml.display()))?;
    let mut out = Vec::new();
    let mut in_features = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
            continue;
        }
        if in_features {
            if let Some(feat) = trimmed.split('=').next() {
                let feat = feat.trim();
                if let Some(name) = feat.strip_prefix("embed-") {
                    if !name.is_empty() {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Discover the C++ official extensions that can be embedded into libduckdb,
/// by scanning `cmake/wasm-extension-config.cmake` for `embed_ext(<name> …)`
/// calls (the gated `duckdb_extension_load` wrapper).
fn discover_cpp_extensions(repo_root: &Path) -> Result<Vec<String>> {
    let cfg = repo_root.join("cmake/wasm-extension-config.cmake");
    let text = std::fs::read_to_string(&cfg)
        .map_err(|e| anyhow::anyhow!("read {}: {e} (pass --repo-root)", cfg.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("embed_ext(") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Parse a `--load NAME=PATH` (or bare `PATH`, whose file stem becomes the
/// name) into `(name, path)`.
fn parse_handler_load(entry: &str) -> Result<(String, PathBuf)> {
    let (name, path) = match entry.split_once('=') {
        Some((n, p)) => (n.to_string(), PathBuf::from(p)),
        None => {
            let path = PathBuf::from(entry);
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("--load {entry}: no file stem for a name"))?
                .to_string();
            (stem, path)
        }
    };
    if !path.is_file() {
        anyhow::bail!("--load {entry}: not a file: {}", path.display());
    }
    Ok((name, path))
}

/// Parse a `--env KEY=VALUE` (explicit) or `--env KEY` (inherit from process
/// env; error if unset) into `(key, value)`.
fn parse_handler_envs(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let (k, v) = match entry.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let val = std::env::var(entry)
                    .map_err(|_| anyhow::anyhow!("--env {entry}: not set in process env"))?;
                (entry.clone(), val)
            }
        };
        if k.is_empty() {
            anyhow::bail!("--env {entry}: empty key");
        }
        out.push((k, v));
    }
    Ok(out)
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
    // `ducklink precompile <in.wasm> <out.cwasm>` AOT-compiles a component so
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
                eprintln!("usage: ducklink precompile <in.wasm> <out.cwasm>");
                std::process::exit(2);
            }
        }
    }

    // `ducklink compose --list`
    // `ducklink compose --embed NAME[,NAME...] [--output PATH] [--precompile] [--repo-root DIR]`
    // Build a custom core component with selected extensions EMBEDDED at compile
    // time (the embed framework: native registration, no WIT boundary, works in
    // the standalone with no host). By default the core embeds nothing extra and
    // extensions load at runtime via `LOAD <name>` (the plugin path). Mirrors
    // sqlite-wasm's `sqlink compose --embed`.
    if raw.get(1).map(String::as_str) == Some("compose") {
        return run_compose(&raw[2..]);
    }

    // `ducklink ui [--port N] [--online|--console] [--no-open] [--assets DIR] [DB]`
    // The host owns the listening socket (httplib can't listen() in the sandbox)
    // and bridges requests to the core component. Default: the REAL DuckDB UI
    // served offline from captured assets; `--online` proxies ui.duckdb.org;
    // `--console` is the tiny built-in SQL console.
    if raw.get(1).map(String::as_str) == Some("ui") {
        let mut port: u16 = 4213;
        let mut open_browser = true;
        let mut mode = UiMode::Offline;
        let mut db: Option<String> = None;
        let mut assets = std::env::current_dir()?.join("web/duckdb-ui");
        let mut i = 2;
        while i < raw.len() {
            match raw[i].as_str() {
                "--port" => {
                    i += 1;
                    port = raw.get(i).and_then(|s| s.parse().ok()).unwrap_or(port);
                }
                "--assets" => {
                    i += 1;
                    if let Some(d) = raw.get(i) {
                        assets = PathBuf::from(d);
                    }
                }
                "--online" => mode = UiMode::Online,
                "--offline" => mode = UiMode::Offline,
                "--console" => mode = UiMode::Console,
                "--no-open" => open_browser = false,
                other => db = Some(other.to_string()),
            }
            i += 1;
        }

        let artifacts = ComponentArtifacts::resolve_default()?;
        let extensions_dir = std::env::current_dir()?.join("artifacts/extensions");
        set_extension_root(extensions_dir);
        let cwd = std::env::current_dir()?;
        // DuckDB's wasm home is "/", and it creates /.duckdb/extension_data at open.
        // The fs shim resolves "/X" relative to the cwd preopen, so pre-create
        // cwd/.duckdb/extension_data (the shim's mkdir isn't recursive enough).
        std::fs::create_dir_all(cwd.join(".duckdb/extension_data")).ok();
        let preopens: Vec<(&Path, &str)> = vec![(cwd.as_path(), ".")];
        serve_ui(&artifacts, db.as_deref(), port, mode, open_browser, &assets, &preopens)?;
        return Ok(());
    }

    // `ducklink serve [--db PATH] [--bind ADDR] [--port N] [--routes-table T]
    //  [--init-routes] [--tls-self-signed | --tls-cert C --tls-key K]`
    // HTTP/HTTPS server that executes SQL against the wasm core and returns JSON,
    // with a database-driven `routes` table (port of sqlite-wasm-httpd). The host
    // owns the listening socket and runs queries through the core component.
    if raw.get(1).map(String::as_str) == Some("serve") {
        let mut db: Option<String> = None;
        let mut bind = "127.0.0.1".to_string();
        let mut port: u16 = 8080;
        let mut routes_table = "routes".to_string();
        let mut init_routes = false;
        let mut tls_cert: Option<PathBuf> = None;
        let mut tls_key: Option<PathBuf> = None;
        let mut tls_self_signed = false;
        let mut loads: Vec<String> = Vec::new();
        let mut envs: Vec<String> = Vec::new();
        let mut i = 2;
        while i < raw.len() {
            match raw[i].as_str() {
                "--db" => {
                    i += 1;
                    db = raw.get(i).cloned();
                }
                "--load" => {
                    i += 1;
                    if let Some(l) = raw.get(i) {
                        loads.push(l.clone());
                    }
                }
                "--env" => {
                    i += 1;
                    if let Some(e) = raw.get(i) {
                        envs.push(e.clone());
                    }
                }
                "--bind" => {
                    i += 1;
                    if let Some(b) = raw.get(i) {
                        bind = b.clone();
                    }
                }
                "--port" => {
                    i += 1;
                    port = raw.get(i).and_then(|s| s.parse().ok()).unwrap_or(port);
                }
                "--routes-table" => {
                    i += 1;
                    if let Some(t) = raw.get(i) {
                        routes_table = t.clone();
                    }
                }
                "--init-routes" => init_routes = true,
                "--tls-self-signed" => tls_self_signed = true,
                "--tls-cert" => {
                    i += 1;
                    tls_cert = raw.get(i).map(PathBuf::from);
                }
                "--tls-key" => {
                    i += 1;
                    tls_key = raw.get(i).map(PathBuf::from);
                }
                other => anyhow::bail!("ducklink serve: unexpected argument `{other}`"),
            }
            i += 1;
        }

        let tls = if tls_self_signed {
            if tls_cert.is_some() || tls_key.is_some() {
                anyhow::bail!("--tls-self-signed conflicts with --tls-cert/--tls-key");
            }
            TlsMode::SelfSigned
        } else {
            match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => TlsMode::Files { cert, key },
                (None, None) => TlsMode::None,
                _ => anyhow::bail!("--tls-cert and --tls-key must be passed together"),
            }
        };

        let artifacts = ComponentArtifacts::resolve_default()?;
        let extensions_dir = std::env::current_dir()?.join("artifacts/extensions");
        set_extension_root(extensions_dir);
        let cwd = std::env::current_dir()?;
        // DuckDB's wasm home is "/"; pre-create cwd/.duckdb/extension_data (the
        // fs shim resolves "/X" relative to the cwd preopen).
        std::fs::create_dir_all(cwd.join(".duckdb/extension_data")).ok();
        let preopens: Vec<(&Path, &str)> = vec![(cwd.as_path(), ".")];
        // Build the request-handler registry from --load NAME=PATH (kind='wasm'
        // routes dispatch to these). --env KEY[=VAL] forwards env into handlers.
        let handlers = if loads.is_empty() {
            None
        } else {
            let env = parse_handler_envs(&envs)?;
            let mut registry = HandlerRegistry::new(env)?;
            for entry in &loads {
                let (name, path) = parse_handler_load(entry)?;
                registry.register(&name, &path)?;
                eprintln!("duckdb-httpd: loaded handler `{name}` from {}", path.display());
            }
            Some(registry)
        };

        let opts = HttpdOptions {
            db,
            bind,
            port,
            routes_table,
            init_routes,
            tls,
        };
        serve_httpd(&artifacts, &opts, &preopens, handlers)?;
        return Ok(());
    }

    let opts = Opts::parse();

    // The network capability gate (network_grant_allows) reads
    // DUCKLINK_NETWORK_GRANT at extension-instantiation time; the --grant-network
    // flag is convenience that sets it (and wins over an inherited env var).
    if let Some(spec) = &opts.grant_network {
        std::env::set_var("DUCKLINK_NETWORK_GRANT", spec);
    }

    if opts.cli_args.is_empty() {
        anyhow::bail!("pass CLI arguments after `--`, e.g. ducklink -- duckdb-cli :memory:");
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
