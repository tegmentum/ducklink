//! `ducklink extension <subcommand>` (alias `ext`) — ergonomic CLI UX for
//! extension management over the multi-provider catalog + the resolver + the
//! transparent-LOAD install flow.
//!
//! Subcommands:
//!   * `list [--available | --installed]` — browse the catalog or the local dir.
//!   * `search <query>` — fuzzy/substring over name/description/categories/exports.
//!   * `info <name>` — the richest view: exports, contract, every provider, which
//!      one the resolver chooses + WHY, conformance, size, source.
//!   * `install <name>` — resolve, fetch + cache, verify content_digest, report.
//!   * `uninstall <name>` — remove from the local extension dir.
//!
//! This module REUSES the existing spine and does not reimplement resolution:
//!   * the catalog is `registry/index.json` (or `DUCKLINK_CATALOG` / the R2
//!     `DUCKLINK_CATALOG_URL` when set);
//!   * `crate::resolver` runs the multi-provider candidate pipeline + conformance
//!     gate, and its `render_reasoning` provides the friendly "WHY" text;
//!   * the install flow drives `native-extension/ducklink/tooling/ducklink-install.sh`
//!     for native shims, and copies the resolved wasm component for wasm providers;
//!   * `compose_core::blobs::compute_digest` verifies `content_digest`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::resolver::{
    self, render_reasoning, ContentRef, Env, Outcome, ProviderKind, ResolvePolicy,
};

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

const HELP: &str = "\
ducklink extension — manage DuckLink componentized extensions

USAGE:
    ducklink extension <SUBCOMMAND>
    ducklink ext <SUBCOMMAND>          (alias)

SUBCOMMANDS:
    list [--available|--installed]   List extensions (catalog or local dir)
    search <QUERY>                   Fuzzy search name/description/category/exports
    info <NAME>                      Full detail: exports, contract, providers, why
    install <NAME>                   Resolve, fetch, verify, and cache an extension
    uninstall <NAME>                 Remove an extension from the local dir

GLOBAL OPTIONS:
    --catalog <PATH>        Override the catalog file (registry/index.json)
    --extensions-dir <DIR>  Local extension directory (default: artifacts/extensions)
    --provider <ID>         Force a specific provider id (resolver override)
    --deny <ID[,ID...]>     Exclude provider ids from resolution
    --json                  Machine-readable JSON output (where supported)
    -h, --help              Show this help

ENVIRONMENT:
    DUCKLINK_CATALOG        Path to a catalog file (overridden by --catalog)
    DUCKLINK_CATALOG_URL    HTTPS URL of a published catalog (R2); fetched read-only
    DUCKLINK_EXTENSIONS_DIR Local extension directory (overridden by --extensions-dir)

EXAMPLES:
    ducklink ext list --available
    ducklink ext search valid
    ducklink ext info aba
    ducklink ext install aba
    ducklink ext list --installed
    ducklink ext uninstall aba
";

// ---------------------------------------------------------------------------
// Parsed options
// ---------------------------------------------------------------------------

#[derive(Default)]
struct GlobalOpts {
    catalog: Option<PathBuf>,
    extensions_dir: Option<PathBuf>,
    provider: Option<String>,
    deny: Vec<String>,
    json: bool,
}

/// Entry point: `args` is everything after `ducklink extension` / `ducklink ext`.
pub fn run(args: &[String]) -> Result<()> {
    // Pull the subcommand out first; global options may appear before or after.
    let mut sub: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut g = GlobalOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" | "help" => {
                print!("{HELP}");
                return Ok(());
            }
            "--catalog" => {
                i += 1;
                g.catalog = Some(PathBuf::from(
                    args.get(i).ok_or_else(|| anyhow!("--catalog expects a path"))?,
                ));
            }
            "--extensions-dir" => {
                i += 1;
                g.extensions_dir = Some(PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow!("--extensions-dir expects a path"))?,
                ));
            }
            "--provider" => {
                i += 1;
                g.provider = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow!("--provider expects an id"))?
                        .clone(),
                );
            }
            "--deny" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| anyhow!("--deny expects id[,id]"))?;
                g.deny = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--json" => g.json = true,
            other if sub.is_none() => sub = Some(other.to_string()),
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    let sub = match sub {
        Some(s) => s,
        None => {
            print!("{HELP}");
            return Ok(());
        }
    };

    match sub.as_str() {
        "list" | "ls" => cmd_list(&g, &positionals),
        "search" | "find" => cmd_search(&g, &positionals),
        "info" | "show" => cmd_info(&g, &positionals),
        "install" | "add" => cmd_install(&g, &positionals),
        "uninstall" | "remove" | "rm" => cmd_uninstall(&g, &positionals),
        other => {
            bail!("unknown subcommand `{other}` (see `ducklink ext --help`)");
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog loading
// ---------------------------------------------------------------------------

struct Catalog {
    value: Value,
    /// Human-readable provenance, e.g. the file path or the URL.
    source: String,
}

impl Catalog {
    fn extensions(&self) -> Vec<&Value> {
        self.value
            .get("extensions")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }

    fn find(&self, name: &str) -> Option<&Value> {
        self.extensions()
            .into_iter()
            .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))
    }

    fn category_desc(&self, cat: &str) -> Option<String> {
        self.value
            .get("categories")
            .and_then(|c| c.get(cat))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// Resolve the catalog path: `--catalog` > `DUCKLINK_CATALOG` env > cwd
/// `registry/index.json` > the bundled workspace catalog. `DUCKLINK_CATALOG_URL`
/// (HTTPS) is fetched read-only when set and no local override applies.
fn load_catalog(g: &GlobalOpts) -> Result<Catalog> {
    if let Some(p) = &g.catalog {
        return read_catalog_file(p);
    }
    if let Some(p) = std::env::var_os("DUCKLINK_CATALOG") {
        return read_catalog_file(Path::new(&p));
    }
    if let Ok(url) = std::env::var("DUCKLINK_CATALOG_URL") {
        if !url.is_empty() {
            return fetch_catalog_url(&url);
        }
    }
    let cwd = std::env::current_dir()?.join("registry/index.json");
    if cwd.is_file() {
        return read_catalog_file(&cwd);
    }
    let bundled = crate::workspace_root().join("registry/index.json");
    read_catalog_file(&bundled)
}

fn read_catalog_file(path: &Path) -> Result<Catalog> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read catalog {}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parse catalog {}", path.display()))?;
    Ok(Catalog {
        value,
        source: path.display().to_string(),
    })
}

/// Fetch a published catalog over HTTPS (the R2 catalog URL). HTTPS only.
fn fetch_catalog_url(url: &str) -> Result<Catalog> {
    if !url.starts_with("https://") {
        bail!("DUCKLINK_CATALOG_URL must be an https:// URL (got {url})");
    }
    let body = reqwest::blocking::get(url)
        .with_context(|| format!("fetch catalog {url}"))?
        .error_for_status()
        .with_context(|| format!("catalog {url}"))?
        .text()
        .with_context(|| format!("read catalog body {url}"))?;
    let value: Value =
        serde_json::from_str(&body).with_context(|| format!("parse catalog {url}"))?;
    Ok(Catalog {
        value,
        source: url.to_string(),
    })
}

/// Resolve the local extension directory: `--extensions-dir` >
/// `DUCKLINK_EXTENSIONS_DIR` env > cwd `artifacts/extensions`.
fn extensions_dir(g: &GlobalOpts) -> Result<PathBuf> {
    if let Some(d) = &g.extensions_dir {
        return Ok(abs(d)?);
    }
    if let Some(d) = std::env::var_os("DUCKLINK_EXTENSIONS_DIR") {
        return Ok(abs(Path::new(&d))?);
    }
    Ok(std::env::current_dir()?.join("artifacts/extensions"))
}

fn abs(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn policy(g: &GlobalOpts) -> ResolvePolicy {
    ResolvePolicy {
        forced_provider: g.provider.clone(),
        denied: g.deny.clone(),
    }
}

/// The resolver environment for CLI resolution: a wasm runtime is always
/// present, and the component-dependency gate sees exactly the shared
/// components the orchestrator can resolve at runtime (parsed from
/// `DUCKLINK_PROVIDERS`). So `ext info`/`ext install` gate a dependent
/// extension the same way a live load would.
fn resolver_env() -> Env {
    Env {
        available_components: resolver::available_components_from_env(),
        ..Env::default()
    }
}

// ---------------------------------------------------------------------------
// Small string + table helpers (duckbox-style)
// ---------------------------------------------------------------------------

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn arr_join(v: &Value, key: &str, sep: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(sep)
        })
        .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Render a duckbox-style table: a header row, a rule, then the body. Column
/// widths size to the widest visible cell (display width approximated by char
/// count — fine for the ASCII catalog text).
fn render_table(headers: &[&str], rows: &[Vec<String>]) {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (c, cell) in row.iter().enumerate().take(cols) {
            let w = cell.chars().count();
            if w > widths[c] {
                widths[c] = w;
            }
        }
    }
    let rule = |left: &str, mid: &str, right: &str| {
        let mut line = String::from(left);
        for (c, w) in widths.iter().enumerate() {
            line.push_str(&"─".repeat(w + 2));
            line.push_str(if c + 1 == cols { right } else { mid });
        }
        line
    };
    let fmt_row = |cells: &[String]| {
        let mut line = String::from("│");
        for (c, w) in widths.iter().enumerate() {
            let cell = cells.get(c).map(String::as_str).unwrap_or("");
            let pad = w - cell.chars().count();
            line.push(' ');
            line.push_str(cell);
            line.push_str(&" ".repeat(pad));
            line.push_str(" │");
        }
        line
    };

    println!("{}", rule("┌", "┬", "┐"));
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", fmt_row(&header_cells));
    println!("{}", rule("├", "┼", "┤"));
    for row in rows {
        println!("{}", fmt_row(row));
    }
    println!("{}", rule("└", "┴", "┘"));
}

// ---------------------------------------------------------------------------
// Conformance summary for catalog views (the resolver owns the real gate; this
// is the human-readable per-extension headline derived from the providers[]).
// ---------------------------------------------------------------------------

/// "certified" if every certifiable provider passes, "partial" if some do,
/// "by-construction" for a bare reference wasm with no record, "-" otherwise.
fn conformance_summary(entry: &Value) -> String {
    let providers = entry.get("providers").and_then(|v| v.as_array());
    let Some(providers) = providers else {
        // backward-compat single-artifact entry: reference wasm by construction.
        return "by-construction".to_string();
    };
    let mut passed = 0usize;
    let mut total = 0usize;
    for p in providers {
        if let Some(c) = p.get("conformance") {
            total += 1;
            if c.get("passed").and_then(|v| v.as_bool()).unwrap_or(false) {
                passed += 1;
            }
        }
    }
    if total == 0 {
        // no records; a reference wasm provider is certified by construction.
        if providers
            .iter()
            .any(|p| p.get("reference").and_then(|v| v.as_bool()).unwrap_or(false))
        {
            "by-construction".to_string()
        } else {
            "-".to_string()
        }
    } else if passed == total {
        format!("certified ({passed}/{total})")
    } else if passed > 0 {
        format!("partial ({passed}/{total})")
    } else {
        format!("failed (0/{total})")
    }
}

/// Compact "providers" cell: e.g. "wasm,native" with the count.
fn providers_kinds(entry: &Value) -> String {
    match entry.get("providers").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => {
            let kinds: Vec<&str> = arr
                .iter()
                .map(|p| p.get("kind").and_then(|v| v.as_str()).unwrap_or("wasm"))
                .collect();
            kinds.join(",")
        }
        // backward-compat single artifact => one wasm provider.
        _ if entry.get("artifact").is_some() => "wasm".to_string(),
        _ => "-".to_string(),
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn cmd_list(g: &GlobalOpts, pos: &[String]) -> Result<()> {
    let mut installed = false;
    let mut available = false;
    for a in pos {
        match a.as_str() {
            "--installed" => installed = true,
            "--available" => available = true,
            other => bail!("ext list: unexpected argument `{other}`"),
        }
    }
    // Default to --available.
    if !installed && !available {
        available = true;
    }
    if installed {
        list_installed(g)
    } else {
        list_available(g)
    }
}

fn list_available(g: &GlobalOpts) -> Result<()> {
    let catalog = load_catalog(g)?;
    let mut exts = catalog.extensions();
    exts.sort_by_key(|e| s(e, "name").to_string());

    if g.json {
        let items: Vec<Value> = exts
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": s(e, "name"),
                    "version": s(e, "version"),
                    "description": s(e, "description"),
                    "categories": e.get("categories").cloned().unwrap_or(Value::Null),
                    "providers": providers_kinds(e),
                    "conformance": conformance_summary(e),
                    "status": s(e, "status"),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    let rows: Vec<Vec<String>> = exts
        .iter()
        .map(|e| {
            vec![
                s(e, "name").to_string(),
                s(e, "version").to_string(),
                truncate(&arr_join(e, "categories", ","), 24),
                providers_kinds(e),
                conformance_summary(e),
                truncate(s(e, "description"), 50),
            ]
        })
        .collect();
    render_table(
        &["name", "version", "categories", "providers", "conformance", "description"],
        &rows,
    );
    println!(
        "{} extensions in catalog ({})",
        exts.len(),
        catalog.source
    );
    Ok(())
}

fn list_installed(g: &GlobalOpts) -> Result<()> {
    let dir = extensions_dir(g)?;
    // The catalog is best-effort here (annotate names we recognize).
    let catalog = load_catalog(g).ok();

    let mut entries: Vec<(String, u64)> = Vec::new();
    if dir.is_dir() {
        for ent in std::fs::read_dir(&dir)
            .with_context(|| format!("read extension dir {}", dir.display()))?
        {
            let ent = ent?;
            let path = ent.path();
            // wasm components and native shims both live here.
            let is_ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "wasm" || e == "duckdb_extension")
                .unwrap_or(false);
            if !is_ext {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push((name, size));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if g.json {
        let items: Vec<Value> = entries
            .iter()
            .map(|(name, size)| {
                serde_json::json!({
                    "name": name,
                    "size": size,
                    "in_catalog": catalog.as_ref().map(|c| c.find(name).is_some()).unwrap_or(false),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No installed extensions in {}", dir.display());
        return Ok(());
    }
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|(name, size)| {
            let in_cat = catalog
                .as_ref()
                .map(|c| if c.find(name).is_some() { "yes" } else { "no" })
                .unwrap_or("?");
            vec![name.clone(), human_size(*size), in_cat.to_string()]
        })
        .collect();
    render_table(&["name", "size", "in-catalog"], &rows);
    println!("{} installed in {}", entries.len(), dir.display());
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

fn cmd_search(g: &GlobalOpts, pos: &[String]) -> Result<()> {
    let query = pos
        .first()
        .ok_or_else(|| anyhow!("usage: ducklink ext search <query>"))?
        .to_lowercase();
    let catalog = load_catalog(g)?;

    let mut hits: Vec<(&Value, i32)> = Vec::new();
    for e in catalog.extensions() {
        let score = search_score(e, &query);
        if score > 0 {
            hits.push((e, score));
        }
    }
    // Higher score first, then name.
    hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| s(a.0, "name").cmp(s(b.0, "name"))));

    if g.json {
        let items: Vec<Value> = hits
            .iter()
            .map(|(e, _)| {
                serde_json::json!({
                    "name": s(e, "name"),
                    "description": s(e, "description"),
                    "categories": e.get("categories").cloned().unwrap_or(Value::Null),
                    "exports": e.get("exports").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("No extensions match `{query}`.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = hits
        .iter()
        .map(|(e, _)| {
            vec![
                s(e, "name").to_string(),
                truncate(&arr_join(e, "categories", ","), 20),
                truncate(s(e, "description"), 56),
            ]
        })
        .collect();
    render_table(&["name", "categories", "description"], &rows);
    println!("{} match(es) for `{query}`", hits.len());
    Ok(())
}

/// Weighted substring match: name hit (esp. exact/prefix) ranks above
/// description/category/export hits. 0 means no match.
fn search_score(e: &Value, q: &str) -> i32 {
    let name = s(e, "name").to_lowercase();
    let mut score = 0;
    if name == q {
        score += 100;
    } else if name.starts_with(q) {
        score += 60;
    } else if name.contains(q) {
        score += 40;
    }
    if s(e, "description").to_lowercase().contains(q) {
        score += 15;
    }
    if arr_join(e, "categories", " ").to_lowercase().contains(q) {
        score += 20;
    }
    if arr_join(e, "keywords", " ").to_lowercase().contains(q) {
        score += 10;
    }
    if arr_join(e, "exports", " ").to_lowercase().contains(q) {
        score += 25;
    }
    score
}

// ---------------------------------------------------------------------------
// info — the richest view
// ---------------------------------------------------------------------------

fn cmd_info(g: &GlobalOpts, pos: &[String]) -> Result<()> {
    let name = pos
        .first()
        .ok_or_else(|| anyhow!("usage: ducklink ext info <name>"))?;
    let catalog = load_catalog(g)?;
    let entry = catalog
        .find(name)
        .ok_or_else(|| anyhow!("extension `{name}` not found in catalog ({})", catalog.source))?;

    // Run the resolver to find the chosen provider + the per-candidate reasoning.
    let manifest = resolver::read_manifest_entry(&catalog.value, name);
    let canonical = canonical_suite_digest(&catalog, name);
    let resolution = manifest.as_ref().map(|m| {
        resolver::resolve(m, &resolver_env(), &policy(g), canonical.as_deref())
    });

    if g.json {
        let chosen = match &resolution {
            Some(Ok(r)) => serde_json::json!({
                "id": r.chosen_id,
                "kind": r.chosen_kind,
                "reasoning": render_reasoning(&r.reasoning),
            }),
            Some(Err(err)) => serde_json::json!({
                "error": err.to_string(),
                "reasoning": render_reasoning(&err.reasoning),
            }),
            None => Value::Null,
        };
        let out = serde_json::json!({
            "name": s(entry, "name"),
            "version": s(entry, "version"),
            "description": s(entry, "description"),
            "categories": entry.get("categories").cloned().unwrap_or(Value::Null),
            "exports": entry.get("exports").cloned().unwrap_or(Value::Null),
            "requires": entry.get("requires").cloned().unwrap_or(Value::Null),
            "requires_components": entry.get("requires_components").cloned().unwrap_or(Value::Null),
            "wit_contract": s(entry, "wit_contract"),
            "wit_contract_version": s(entry, "wit_contract_version"),
            "source": s(entry, "source"),
            "providers": entry.get("providers").cloned().unwrap_or(Value::Null),
            "chosen": chosen,
            "installed": is_installed(g, name),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Header block.
    println!("{}  v{}", s(entry, "name"), s(entry, "version"));
    let desc = s(entry, "description");
    if !desc.is_empty() {
        println!("  {desc}");
    }
    println!();

    let cats = arr_join(entry, "categories", ", ");
    if !cats.is_empty() {
        let detail = entry
            .get("categories")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.as_str())
            .and_then(|c| catalog.category_desc(c));
        match detail {
            Some(d) => println!("  Categories : {cats} — {d}"),
            None => println!("  Categories : {cats}"),
        }
    }
    println!("  Source     : {}", s(entry, "source"));
    let contract = s(entry, "wit_contract");
    println!(
        "  Contract   : {} @ {}",
        crate::resolver::short_digest_pub(contract),
        s(entry, "wit_contract_version")
    );
    let requires = arr_join(entry, "requires", ", ");
    if !requires.is_empty() {
        println!("  Capabilities: {requires}");
    }
    // GENERAL component-dependency graph: the other components this one resolves
    // at runtime via the orchestrator (resident, shared). Dependency + capability
    // transparency for the marketplace.
    let requires_components = arr_join(entry, "requires_components", ", ");
    if !requires_components.is_empty() {
        let available = resolver::available_components_from_env();
        let status: Vec<String> = entry
            .get("requires_components")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|id| {
                        if available.iter().any(|a| a == id) {
                            format!("{id} (available)")
                        } else {
                            format!("{id} (ABSENT — set DUCKLINK_PROVIDERS={id}=…)")
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        println!("  Requires    : {} [components, runtime-resolved]", status.join(", "));
    }

    // Exports (the registered function/type names).
    let exports = entry.get("exports").and_then(|v| v.as_array());
    if let Some(exports) = exports {
        if !exports.is_empty() {
            println!();
            println!("  Exports ({}):", exports.len());
            for ex in exports {
                if let Some(ex) = ex.as_str() {
                    println!("    - {ex}");
                }
            }
        }
    }

    // Local artifact size (if installed).
    if let Some((path, size)) = installed_artifact(g, name) {
        println!();
        println!("  Installed  : {} ({})", path.display(), human_size(size));
    } else {
        println!();
        println!("  Installed  : no (run `ducklink ext install {name}`)");
    }

    // Providers + resolver decision.
    println!();
    println!("  Providers:");
    let chosen_id = match &resolution {
        Some(Ok(r)) => Some(r.chosen_id.clone()),
        _ => None,
    };
    if let Some(arr) = entry.get("providers").and_then(|v| v.as_array()) {
        for p in arr {
            print_provider(p, chosen_id.as_deref());
        }
    } else if entry.get("artifact").is_some() {
        // backward-compat single artifact.
        println!(
            "    * wasm-component [wasm, reference]  artifact={}  digest={}",
            s(entry, "artifact"),
            crate::resolver::short_digest_pub(s(entry, "content_digest"))
        );
    }

    // The resolver's decision + WHY.
    println!();
    match &resolution {
        Some(Ok(r)) => {
            println!("  Resolver   : chose `{}` [{}]", r.chosen_id, r.chosen_kind);
            println!("    why: {}", render_reasoning(&r.reasoning));
        }
        Some(Err(err)) => {
            println!("  Resolver   : NO admissible provider");
            println!("    why: {}", render_reasoning(&err.reasoning));
        }
        None => {
            println!("  Resolver   : (no usable manifest entry)");
        }
    }
    Ok(())
}

fn print_provider(p: &Value, chosen: Option<&str>) {
    let id = s(p, "id");
    let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("wasm");
    let reference = p.get("reference").and_then(|v| v.as_bool()).unwrap_or(false);
    let marker = if Some(id) == chosen { "* " } else { "  " };
    let mut flags = vec![kind.to_string()];
    if reference {
        flags.push("reference".to_string());
    }

    let conf = p.get("conformance");
    let conf_str = match conf {
        Some(c) => {
            let passed = c.get("passed").and_then(|v| v.as_bool()).unwrap_or(false);
            format!(
                "conformance={} (suite {}, at {})",
                if passed { "passed" } else { "FAILED" },
                short(s(c, "suite_digest")),
                short(s(c, "at"))
            )
        }
        None => "conformance=none".to_string(),
    };

    let artifact = match kind {
        "remote" => s(p, "endpoint").to_string(),
        _ => s(p, "artifact").to_string(),
    };
    println!("  {marker}{id} [{}]", flags.join(", "));
    if !artifact.is_empty() {
        println!("        artifact: {artifact}");
    }
    if kind == "native" {
        if let Some(plat) = p.get("platform") {
            println!(
                "        platform: {}/{}",
                s(plat, "os"),
                s(plat, "arch")
            );
        }
    }
    if let Some(digest) = p.get("content_digest").and_then(|v| v.as_str()) {
        println!("        digest:   {}", short(digest));
    }
    println!("        {conf_str}");
}

fn short(d: &str) -> String {
    d.chars().take(12).collect()
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

fn cmd_install(g: &GlobalOpts, pos: &[String]) -> Result<()> {
    let name = pos
        .first()
        .ok_or_else(|| anyhow!("usage: ducklink ext install <name>"))?;
    let catalog = load_catalog(g)?;

    let manifest = resolver::read_manifest_entry(&catalog.value, name).ok_or_else(|| {
        anyhow!("extension `{name}` not found (or has no usable provider) in {}", catalog.source)
    })?;
    let canonical = canonical_suite_digest(&catalog, name);
    let resolution = resolver::resolve(&manifest, &resolver_env(), &policy(g), canonical.as_deref())
        .map_err(|err| {
            // Surface the resolver's friendly reasoning (uncertified /
            // contract-mismatch / forced-out / unavailable substrate).
            anyhow!(
                "cannot install `{name}`: {}\n  reasoning: {}",
                err,
                render_reasoning(&err.reasoning)
            )
        })?;

    let dir = extensions_dir(g)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create extension dir {}", dir.display()))?;

    eprintln!(
        "[install] {name}: resolver chose `{}` [{}]",
        resolution.chosen_id, resolution.chosen_kind
    );

    let chosen = manifest
        .providers
        .iter()
        .find(|p| p.id == resolution.chosen_id)
        .expect("chosen provider present in manifest");

    match &chosen.kind {
        ProviderKind::Wasm {
            content_digest, ..
        } => {
            let src = resolve_wasm_source(&catalog, &resolution.artifact)?;
            let dest = dir.join(format!("{name}.wasm"));
            install_wasm(&src, &dest, content_digest.as_deref())?;
            eprintln!(
                "[install] wrote {} ({})",
                dest.display(),
                human_size(std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0))
            );
        }
        ProviderKind::Native { os, arch, .. } => {
            // Drive the transparent-LOAD install flow (ducklink-install.sh).
            install_native(&catalog, name, &dir, os, arch)?;
        }
        ProviderKind::Remote { endpoint } => {
            bail!("remote provider `{}` ({endpoint}) is not installable to a local dir; it is a hosted endpoint", resolution.chosen_id);
        }
    }

    println!(
        "Installed `{name}` -> provider `{}` [{}], contract {} @ {}",
        resolution.chosen_id,
        resolution.chosen_kind,
        crate::resolver::short_digest_pub(&manifest.wit_contract),
        s(catalog.find(name).unwrap_or(&Value::Null), "wit_contract_version")
    );
    println!("Run it with:  LOAD {name};   (or `ducklink -- duckdb-cli :memory:` then LOAD {name};)");
    Ok(())
}

/// Resolve a wasm provider's artifact reference to a readable source path.
/// `ContentRef::Path` is taken relative to the catalog's directory / cwd /
/// workspace; a `Digest`/`Oci` reference is not fetchable in this pass.
fn resolve_wasm_source(catalog: &Catalog, artifact: &ContentRef) -> Result<PathBuf> {
    match artifact {
        ContentRef::Path(rel) => {
            if rel.is_absolute() && rel.is_file() {
                return Ok(rel.clone());
            }
            // Try cwd, then the workspace root (where the bundled artifacts live).
            let cwd = std::env::current_dir()?.join(rel);
            if cwd.is_file() {
                return Ok(cwd);
            }
            let ws = crate::workspace_root().join(rel);
            if ws.is_file() {
                return Ok(ws);
            }
            bail!(
                "artifact {} not found (tried cwd + workspace; catalog {})",
                rel.display(),
                catalog.source
            );
        }
        ContentRef::Digest(d) => {
            bail!("digest-only artifact {d} is not fetchable in this build (no OCI client wired)")
        }
        ContentRef::Oci(r) => {
            bail!("OCI artifact {r} is not fetchable in this build (no OCI client wired)")
        }
    }
}

/// Copy the wasm component into the extension dir, verifying `content_digest`.
fn install_wasm(src: &Path, dest: &Path, expected_digest: Option<&str>) -> Result<()> {
    let bytes = std::fs::read(src).with_context(|| format!("read {}", src.display()))?;
    if let Some(expected) = expected_digest {
        if !expected.is_empty() {
            let actual: String = compose_core::blobs::compute_digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if actual != expected {
                bail!(
                    "content_digest mismatch for {}:\n  expected {}\n  actual   {}",
                    src.display(),
                    expected,
                    actual
                );
            }
            eprintln!("[install] verified content_digest {}", short(expected));
        }
    } else {
        eprintln!("[install] no content_digest in catalog entry (skipping verification)");
    }
    std::fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

/// Drive the transparent-LOAD native install flow (`ducklink-install.sh`), which
/// runs stock DuckDB's `INSTALL <name> FROM '<repo>'` to populate the extension
/// dir so a plain `LOAD <name>` works thereafter.
fn install_native(
    _catalog: &Catalog,
    name: &str,
    dir: &Path,
    os: &str,
    arch: &str,
) -> Result<()> {
    let script = crate::workspace_root()
        .join("native-extension/ducklink/tooling/ducklink-install.sh");
    if !script.is_file() {
        bail!(
            "native install flow unavailable: {} not found",
            script.display()
        );
    }
    // The custom-repo layout for native shims lives alongside the script.
    let repo = crate::workspace_root().join("native-extension/ducklink");
    let cli = std::env::var("DUCKLINK_DUCKDB_CLI").unwrap_or_else(|_| "duckdb".to_string());
    eprintln!(
        "[install] native ({os}/{arch}): {} {name} {} {}",
        script.display(),
        repo.display(),
        dir.display()
    );
    let status = std::process::Command::new("bash")
        .arg(&script)
        .arg(name)
        .arg(&repo)
        .arg(dir)
        .arg(&cli)
        .status()
        .with_context(|| format!("spawn {}", script.display()))?;
    if !status.success() {
        bail!(
            "native install of `{name}` failed (set DUCKLINK_DUCKDB_CLI if `duckdb` is not on PATH)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

fn cmd_uninstall(g: &GlobalOpts, pos: &[String]) -> Result<()> {
    let name = pos
        .first()
        .ok_or_else(|| anyhow!("usage: ducklink ext uninstall <name>"))?;
    let dir = extensions_dir(g)?;
    let mut removed = Vec::new();
    for ext in ["wasm", "duckdb_extension"] {
        let path = dir.join(format!("{name}.{ext}"));
        if path.is_file() {
            std::fs::remove_file(&path)
                .with_context(|| format!("remove {}", path.display()))?;
            removed.push(path);
        }
    }
    if removed.is_empty() {
        bail!(
            "`{name}` is not installed in {} (nothing to remove)",
            dir.display()
        );
    }
    for p in &removed {
        println!("Removed {}", p.display());
    }
    println!("Uninstalled `{name}`.");
    Ok(())
}

// ---------------------------------------------------------------------------
// shared helpers (installed-state, canonical suite digest)
// ---------------------------------------------------------------------------

fn is_installed(g: &GlobalOpts, name: &str) -> bool {
    installed_artifact(g, name).is_some()
}

fn installed_artifact(g: &GlobalOpts, name: &str) -> Option<(PathBuf, u64)> {
    let dir = extensions_dir(g).ok()?;
    for ext in ["wasm", "duckdb_extension"] {
        let path = dir.join(format!("{name}.{ext}"));
        if let Ok(meta) = std::fs::metadata(&path) {
            return Some((path, meta.len()));
        }
    }
    None
}

/// The canonical conformance `suite_digest` the resolver should hold for this
/// extension, mirroring the host's `prepare_extension` policy: derive it from the
/// providers' own (agreed) conformance record. When the providers all pin the
/// same `suite_digest`, that is the canonical the gate enforces; otherwise `None`
/// (the un-promoted long tail, reference-by-construction fallback).
fn canonical_suite_digest(catalog: &Catalog, name: &str) -> Option<String> {
    let entry = catalog.find(name)?;
    let providers = entry.get("providers").and_then(|v| v.as_array())?;
    let mut digest: Option<String> = None;
    for p in providers {
        let d = p
            .get("conformance")
            .and_then(|c| c.get("suite_digest"))
            .and_then(|v| v.as_str());
        if let Some(d) = d {
            if d.is_empty() {
                continue;
            }
            match &digest {
                None => digest = Some(d.to_string()),
                Some(existing) if existing != d => return None, // disagreement
                _ => {}
            }
        }
    }
    digest
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn human_size_rounds() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(112128), "109.5 KiB");
    }

    #[test]
    fn search_score_weights_name_over_description() {
        let exact = json!({"name": "aba", "description": "x"});
        let prefix = json!({"name": "abacus", "description": "x"});
        let desc = json!({"name": "zzz", "description": "the aba thing"});
        let none = json!({"name": "zzz", "description": "nope"});
        assert!(search_score(&exact, "aba") > search_score(&prefix, "aba"));
        assert!(search_score(&prefix, "aba") > search_score(&desc, "aba"));
        assert_eq!(search_score(&none, "aba"), 0);
    }

    #[test]
    fn search_score_matches_exports_and_categories() {
        let e = json!({
            "name": "x", "description": "d",
            "categories": ["validators"], "exports": ["foo_validate"],
        });
        assert!(search_score(&e, "validators") > 0);
        assert!(search_score(&e, "foo_validate") > 0);
    }

    #[test]
    fn conformance_summary_kinds() {
        // explicit providers, all passed
        let all = json!({"providers": [
            {"id": "a", "kind": "wasm", "conformance": {"passed": true}},
            {"id": "b", "kind": "native", "conformance": {"passed": true}},
        ]});
        assert_eq!(conformance_summary(&all), "certified (2/2)");
        // reference wasm, no records
        let by = json!({"providers": [{"id": "a", "kind": "wasm", "reference": true}]});
        assert_eq!(conformance_summary(&by), "by-construction");
        // backward-compat single artifact
        let bc = json!({"artifact": "x.wasm"});
        assert_eq!(conformance_summary(&bc), "by-construction");
        // partial
        let partial = json!({"providers": [
            {"id": "a", "kind": "wasm", "conformance": {"passed": true}},
            {"id": "b", "kind": "native", "conformance": {"passed": false}},
        ]});
        assert_eq!(conformance_summary(&partial), "partial (1/2)");
    }

    #[test]
    fn providers_kinds_compact() {
        let e = json!({"providers": [
            {"id": "a", "kind": "wasm"}, {"id": "b", "kind": "native"},
        ]});
        assert_eq!(providers_kinds(&e), "wasm,native");
        let bc = json!({"artifact": "x.wasm"});
        assert_eq!(providers_kinds(&bc), "wasm");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }
}
