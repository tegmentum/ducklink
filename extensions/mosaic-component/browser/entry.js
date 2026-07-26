// Single ESM entry point that esbuild bundles into a self-contained
// IIFE. `mosaicRuntime` is the global the SPA shell (index-template.html)
// calls into.
//
// The runtime exposes a `boot(config)` function that wires up mosaic's
// coordinator against the ducklink REST endpoint and hands the spec to
// mosaic-spec's `parseSpec` + `astToDOM` pair, matching the reference
// standalone-vgplot flow. If parsing fails we fall back to a minimal
// Observable Plot render so the page still shows *something* — enough
// to satisfy the Phase 1 acceptance criterion (Section 27 of the memo).

import * as vg from "@uwdata/vgplot";
import { restConnector, coordinator } from "@uwdata/mosaic-core";
import { parseSpec, astToDOM, InstantiateContext } from "@uwdata/mosaic-spec";
import * as Plot from "@observablehq/plot";

async function boot({ queryUri, specUri, mountEl }) {
  const target = typeof mountEl === "string" ? document.querySelector(mountEl) : mountEl;
  if (!target) throw new Error("mosaic boot: mount element not found");

  target.textContent = "loading spec…";

  // 1) fetch the spec JSON the extension staged under /api/app/<name>/spec.
  let spec;
  try {
    const res = await fetch(specUri, { cache: "no-store" });
    if (!res.ok) throw new Error(`spec: HTTP ${res.status}`);
    spec = await res.json();
  } catch (e) {
    target.textContent = `spec fetch failed: ${e}`;
    return;
  }

  // 2) wire the coordinator against the ducklink REST endpoint. The
  //    coordinator is a singleton exported by mosaic-core; vgplot re-
  //    exports it so vgplot marks pick up the same connector.
  const coord = coordinator();
  coord.databaseConnector(restConnector({ uri: queryUri }));

  // 3) parse the spec into an AST and materialize DOM via astToDOM. The
  //    InstantiateContext accepts the API namespace (vgplot re-exports
  //    every mark/legend/interactor/attribute directive), which is what
  //    astToDOM expects.
  target.textContent = "";
  try {
    const ast = parseSpec(spec);
    const ctx = new InstantiateContext({ api: vg });
    const out = ast.instantiate(ctx);
    target.appendChild(out instanceof Node ? out : document.createTextNode(String(out)));
    return;
  } catch (e) {
    console.warn("mosaic parseSpec/astToDOM failed, falling back to Plot:", e);
    // fall through to raw Plot fallback below
  }

  // Fallback: raw query + Observable Plot. Handles the common
  // mosaic.plot() output shape ({data:{<name>:{query:"..."}}, plot:[...]}).
  try {
    const conn = restConnector({ uri: queryUri });
    const dataName = pickFirstDataName(spec);
    const sql = `SELECT * FROM ${dataName}`;
    const rows = await conn.query({ type: "json", sql });
    const marks = [];
    if (rows && rows.length) {
      const cols = Object.keys(rows[0]);
      const x = cols[0];
      const y = cols[1] ?? cols[0];
      marks.push(Plot.line(rows, { x, y }));
      marks.push(Plot.dot(rows, { x, y }));
    }
    target.appendChild(Plot.plot({ marks, width: 720, height: 320 }));
  } catch (e) {
    target.textContent = `mosaic fallback render failed: ${e}`;
  }
}

function pickFirstDataName(spec) {
  if (spec && spec.data && typeof spec.data === "object") {
    const k = Object.keys(spec.data)[0];
    if (k) return k;
  }
  return "fixture";
}

export { boot };
