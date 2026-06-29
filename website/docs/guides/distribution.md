---
id: distribution
title: Extension distribution (Cloudflare R2)
sidebar_label: Extension distribution (R2)
---

# Extension distribution — Cloudflare R2

Extension components are distributed from **Cloudflare R2** at the custom domain
`datalink-ext.tegmentum.ai`. R2 charges **zero egress**, and the wasm artifacts
are content-addressed and immutable, so Cloudflare's edge caches them
indefinitely and the publishing host is never in the byte path. A small mutable
`catalog.json` points clients at the right artifact.

## The bucket layout

| Path | Contents | Cache policy |
|---|---|---|
| `wasm/sha256/<digest>/<name>.wasm` | the content-addressed, immutable shared wasm store | `public, max-age=31536000, immutable` |
| `ducklink/catalog.json` | the mutable catalog pointer | `public, max-age=60, must-revalidate` |
| `<revision>/<platform>/<name>.duckdb_extension(.gz)` | the stock-DuckDB `custom_extension_repository` tree | per-revision |

The `ducklink/` and `sqlink/` namespaces are separate catalogs in the same
bucket. The wasm store is shared and keyed by digest, so the same artifact is
fetched once regardless of how many catalogs reference it.

## The catalog model

`catalog.json` is **generated** (`tooling/gen-catalog.py`, via the shared
`datalink_tooling.gen` identity-stamper) and uploaded — it is not checked in. Each
entry carries identity and conformance metadata:

```json
{
  "extensions": [
    {
      "name": "baseN",
      "description": "...",
      "exports": ["base32_encode", "..."],
      "version": "0.1.0",
      "wit_contract": "a2ad9764ac971345d6a650b92edbda034b160980acf148d354126f7e6f92ba40",
      "wit_contract_version": "4.0.0",
      "content_digest": "...",
      "providers": [
        {
          "kind": "wasm",
          "platform": { "os": "any", "arch": "wasm32" },
          "conformance": { "passed": true, "suite": "baseN@4", "at": "a2ad9764..." },
          "content_digest": "...",
          "artifact": "wasm/sha256/<digest>/baseN.wasm"
        }
      ]
    }
  ],
  "categories": { "...": ["..."] }
}
```

The `wit_contract` digest is the [`@4.0.0` contract
identity](../architecture/columnar-abi.md#the-contract-identity-is-a-content-digest);
a client verifies that an artifact's contract matches what it can load before
fetching it.

## Publishing

The upload tool is `ducklink-host`'s `publish` path (`crates/ducklink-host/src/publish.rs`):
SigV4 PUTs to the R2 S3 endpoint, with a content-digest integrity gate and a
`--dry-run` mode. The first live publish runs from the `publish-r2` CI workflow
(`.github/workflows/publish-r2.yml`) after merge. CORS is a one-time bucket
setting (`deploy/r2/cors.json` + `deploy/r2/apply-cors.sh`): `GET` / `HEAD` only,
range + conditional headers allowed, exposing `etag` / `content-length` /
`content-range` / `cache-control`. CORS is a permission header, not a proxy hop,
so the zero-egress property is preserved.

## Pointing a client at the repo

| Client | How it points |
|---|---|
| **Browser / JS API** | fetches `<base>/ducklink/catalog.json` (default base `https://datalink-ext.tegmentum.ai`); override with `window.__CATALOG_BASE` or build env `VITE_CATALOG_BASE`. Artifacts at `<base>/wasm/sha256/<digest>/<name>.wasm`. |
| **CLI / host** | `DUCKLINK_CATALOG_URL=https://datalink-ext.tegmentum.ai/ducklink/catalog.json` |
| **Stock DuckDB** | `SET custom_extension_repository='https://datalink-ext.tegmentum.ai/<revision>/<platform>';` |

:::note Bundled fallback
The browser loader fetches the R2 catalog with `mode: 'cors'` and, on any failure
(e.g. CORS not yet applied for the requesting origin), **transparently falls back
to a bundled local catalog**. So a given session may serve from R2 or from the
bundle depending on whether CORS is live for that origin.
:::
