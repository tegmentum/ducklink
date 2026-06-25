---
id: embedding-tracking
title: Embedding tracking (builds / bundles)
sidebar_label: Embedding tracking
---

# Embedding tracking (builds / bundles)

ducklink tracks **what builds embed which extensions** — sqlink's *Bundles* model,
adapted to ducklink's two embedding layers and stored as JSON. A *build record* is
a named, content-hashed set of embedding members keyed by a `set_hash`.

A ducklink build has two embedding layers:

- **core_embedded** — the wasm core's statically embedded set (`EMBED_EXTENSIONS`
  at build time; the lean default embeds nothing optional — `core_functions` +
  `parquet`).
- **components** — the loaded / autoloaded / **composed** component extensions
  (e.g. `jsonfns` autoloads; `spatialproj` *composes* the GDAL component via
  `wac`).

## Records & tooling

Records live in `registry/builds.json`; the human-readable index is `BUILDS.md`.
The recorder/query tool is `tooling/builds.py`:

```bash
# Record an ad-hoc bundle (core embed set + a component, content-hashed)
python3 tooling/builds.py record lean-default --kind core \
    --embed core_functions,parquet \
    --component jsonfns@artifacts/extensions/jsonfns.wasm

python3 tooling/builds.py list    # NAME | KIND | CORE-EMBEDDED | #COMP | SET-HASH | CREATED
python3 tooling/builds.py show lean-default   # full detail incl. composed-of graph
python3 tooling/builds.py gen     # (re)write BUILDS.md
python3 tooling/builds.py verify  # every set_hash recomputes; every artifact present
```

## How content hashing works

Members are content-hashed with **BLAKE2b-256** (stdlib `hashlib.blake2b`; sqlink
uses blake3, unavailable in the Python stdlib). The `set_hash` is BLAKE2b-256 over
the sorted, newline-terminated `name\thash` member lines — the same named-set /
content-hashed-member / `set_hash` identity as sqlink.

Re-recording an unchanged set is idempotent (`created_at` is preserved); reusing a
name for a *different* set is an error (sqlink's alias-conflict rule).

## Self-recording build hooks

The embedding sets capture themselves:

- The wasm build script
  (`../duckdb-wasm/scripts/build-libduckdb-wasm.sh`) writes
  `registry/last-core-build.json` (the `EMBED_EXTENSIONS` split + the core artifact
  hash) after a successful build (guarded on `$DUCKLINK`).
- `extensions/spatialproj-component/compose.sh` writes `spatialproj.compose.json`
  after `wac plug`, recording the GDAL composition (`gdal` embeds `PROJ`/`proj.db`).

Ingest either with `python3 tooling/builds.py record <name> --from-manifest <file>`.

## The `.bundle` dot command

The `.bundle` dot command (`extensions/bundle-dotcmd/`, in `artifacts/dotcmds/`) is
the interactive surface:

- `.bundle loaded` introspects the live loaded-extension set (via
  `duckdb_extensions()`, a core builtin — works on the lean core, no JSON extension
  needed).
- `.bundle members` renders it as `set_hash` member lines.
- `.bundle help` points at `tooling/builds.py` for the persisted records.

(The dotcmd has no filesystem access, and `read_json` would need the de-embedded
JSON extension — hence the builtin introspection path.)

## Example index

| Build | Kind | Core-embedded | Components | set_hash |
|---|---|---|---|---|
| **lean-default** | core | `core_functions`, `parquet` | `jsonfns` | `fbd54aaa1fda9e33…` |
| **spatialproj** | composed | _(lean)_ | `spatialproj` | `f351e24b53a493fe…` |

Compositions and what each sub-component embeds:

- **spatialproj** composes `gdal` — which embeds `PROJ`, `proj.db`.
