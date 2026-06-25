# ducklink builds — embedding tracking

> Auto-generated from `registry/builds.json` by `python3 tooling/builds.py gen`. Do not edit by hand.

Each **build** is a named, content-hashed set of embedding members (sqlink's *Bundles* model, JSON-storage idiom). A ducklink build has two embedding layers: the wasm **core's** statically embedded set (`EMBED_EXTENSIONS`; lean default embeds nothing optional) and the **component** extensions it loads / autoloads / composes. The `set_hash` (BLAKE2b-256 over the sorted `name\thash` members) keys the set.

**2 build(s) tracked.**

| Build | Kind | Core-embedded | Components | set_hash |
|---|---|---|---|---|
| **lean-default** | core | `core_functions`, `parquet` | `jsonfns` | `fbd54aaa1fda9e33…` |
| **spatialproj** | composed | _(lean)_ | `spatialproj` | `f351e24b53a493fe…` |

## Compositions

Inter-component (`wac plug`) compositions and what each sub-component embeds:

- **spatialproj** composes:
  - `gdal` — embeds `PROJ`, `proj.db`

## How embedding is recorded

- **Core embedding** — the wasm build script (`../duckdb-wasm/scripts/build-libduckdb-wasm.sh`) writes `registry/last-core-build.json` with the `EMBED_EXTENSIONS` split + the core artifact hash after each build. Ingest with `python3 tooling/builds.py record <name> --kind core --from-manifest registry/last-core-build.json`.
- **Compositions** — `extensions/spatialproj-component/compose.sh` writes `extensions/spatialproj-component/spatialproj.compose.json` after `wac plug`. Ingest with `python3 tooling/builds.py record spatialproj --kind composed --from-manifest extensions/spatialproj-component/spatialproj.compose.json`.
- **Ad-hoc bundles** — `python3 tooling/builds.py record <name> --embed a,b --component jsonfns@artifacts/extensions/jsonfns.wasm`.

