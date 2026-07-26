// Bundles @uwdata/vgplot + @uwdata/mosaic-core + friends into a single
// self-contained IIFE that assigns to `window.mosaicRuntime`. The bundle
// is embedded in the wasm component via `include_bytes!` and served from
// the routes table as `/ducklink/mosaic/app/<name>/bundle.js`.
//
// Also stages `dist/index.html` — the SPA shell the extension serves
// from `/ducklink/mosaic/app/<name>` — from `index-template.html`. Copies
// verbatim; the extension substitutes `<TOKEN>` and `<NAME>` placeholders
// at `mosaic.create()` time before writing the row into the routes table.

import * as esbuild from "esbuild";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const DIST = resolve(HERE, "dist");

mkdirSync(DIST, { recursive: true });

await esbuild.build({
  entryPoints: [resolve(HERE, "entry.js")],
  outfile: resolve(DIST, "bundle.js"),
  bundle: true,
  format: "iife",
  globalName: "mosaicRuntime",
  platform: "browser",
  target: ["es2022"],
  minify: true,
  sourcemap: false,
  legalComments: "none",
  logLevel: "info",
});

copyFileSync(resolve(HERE, "index-template.html"), resolve(DIST, "index.html"));
console.log("staged dist/index.html");
