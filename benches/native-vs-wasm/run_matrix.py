#!/usr/bin/env python3
"""Full CONFIGURATION-MATRIX native-vs-wasm benchmark for ducklink extensions.

Extends the head-to-head harness (run.py) from 2 paths to the 5-config matrix:

    1. native               -- bundled DuckDB + the native ducklink extension
    2. wasm-dynamic-noaot    -- wasm core + extension as separate components,
                                host-resolver/WIT-dispatched, JIT at instantiation
    3. wasm-dynamic-aot      -- same, core+cli loaded from precompiled .cwasm
    4. wasm-embedded-noaot   -- extension compiled INTO the core (no WIT dispatch
                                boundary), JIT at instantiation
    5. wasm-embedded-aot     -- embedded + precompiled .cwasm

Same workloads (workloads.json), same SQL, same data, every cell -- so the
matrix is directly comparable. METHODOLOGY is identical to run.py: for each
(config, workload) we run the query K times in one process and time the whole
process externally for several K (first K=0), then fit

        wall_time(K) = intercept + slope * K

by least squares. `slope` is the marginal per-query cost (the THROUGHPUT
number); `intercept`/`fixed_cost_ms` is the one-time cost (process spawn, wasm
compile, load, setup) -- which the slope EXCLUDES by construction. We report
BOTH because the two interesting axes live in different places:

  * EMBEDDED vs DYNAMIC -> shows up in the SLOPE (the per-row dispatch boundary).
  * AOT vs no-AOT       -> shows up in FIXED_COST (the one-time compile), and we
                          verify the slope is unchanged (AOT must not alter
                          steady-state throughput).

A config whose artifacts are missing or whose workload does not resolve is
reported as `skipped` with a reason rather than aborting the run, so a partial
matrix (e.g. when the embed framework is not built) is still publishable.

Artifact roots (env):
    NVW_ART     dir with ducklink_core.wasm, ducklink_cli.wasm, extensions/<n>.wasm
                (default ./artifacts-3.1.0)
    NVW_AOT     dir with ducklink_core.cwasm, ducklink_cli.cwasm
                (default $NVW_ART/aot)
    NVW_EMBED   dir with <ext>/ducklink_core.wasm (+ .cwasm) -- a core per
                embedded extension (default $NVW_ART/embedded)
    HOST_BIN    ducklink host binary
    NATIVE_BIN  nvw_native_runner binary
    CONTRACT_LABEL  report label (default from matrix.json)

Usage:
    ./run_matrix.py                       # full matrix -> results/ + MATRIX.md
    ./run_matrix.py --quick               # 2 reps (smoke)
    ./run_matrix.py --only aba_validate_1m
    ./run_matrix.py --configs native,wasm-dynamic-noaot
    ./run_matrix.py --verify-only         # just report which cells resolve
"""

import argparse
import json
import math
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ART = Path(os.environ.get("NVW_ART", str(HERE / "artifacts-3.1.0")))
AOT = Path(os.environ.get("NVW_AOT", str(ART / "aot")))
EMBED = Path(os.environ.get("NVW_EMBED", str(ART / "embedded")))
HOST_BIN = Path(os.environ.get("HOST_BIN", str(Path.home() / "git" / "ducklink" / "target" / "release" / "ducklink")))
NATIVE_BIN = Path(os.environ.get(
    "NATIVE_BIN",
    str(HERE / ".." / ".." / "native-extension" / "ducklink" / "target" / "release" / "nvw_native_runner")))
TIMEOUT = int(os.environ.get("NVW_TIMEOUT", "900"))

# Generic, workload-independent failure markers. NOTE: the host always tries to
# autoload DuckDB built-ins (e.g. jsonfns) and prints a harmless
# "host declined extension 'jsonfns'" / "extension instantiation for jsonfns
# failed" -- those must NOT count. So extension-load failures are matched
# ext-SCOPED (see verify_cell), and only true query errors are generic here.
ERROR_MARKERS = [
    "panicked", "Catalog Error", "Binder Error", "Parser Error",
    "did not find function", "Function with name",
]


def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


# --- artifact resolution per config ---------------------------------------

def core_for(cfg, wl):
    """Return the core component path for a wasm config + workload, or None."""
    if cfg.get("embedded"):
        base = EMBED / wl["extension"]
        p = base / ("ducklink_core.cwasm" if cfg.get("aot") else "ducklink_core.wasm")
        return p if p.exists() else None
    if cfg.get("aot"):
        p = AOT / "ducklink_core.cwasm"
        return p if p.exists() else None
    p = ART / "ducklink_core.wasm"
    return p if p.exists() else None


def cli_for(cfg):
    if cfg.get("aot"):
        p = AOT / "ducklink_cli.cwasm"
        if p.exists():
            return p
    p = ART / "ducklink_cli.wasm"
    return p if p.exists() else None


def ext_dir():
    return ART / "extensions"


# --- runners ---------------------------------------------------------------

def native_cmd(wl, k):
    cmd = [str(NATIVE_BIN),
           "--component", str(ext_dir() / wl["component"]),
           "--name", wl["extension"],
           "--setup", wl["setup"],
           "--query", wl["query"],
           "--iters", str(k)]
    if wl.get("needs_raw"):
        cmd.append("--raw")
    return cmd


def wasm_cmd(cfg, wl):
    core = core_for(cfg, wl)
    cli = cli_for(cfg)
    cmd = [str(HOST_BIN)]
    if core:
        cmd += ["--core-component", str(core)]
    if cli:
        cmd += ["--cli-component", str(cli)]
    if not cfg.get("embedded"):
        cmd += ["--extensions-dir", str(ext_dir())]
    cmd += ["--", ":memory:"]
    if not cfg.get("embedded"):
        cmd += ["--load-extension", wl["extension"]]
    return cmd


def run_native(cfg, wl, k):
    t0 = time.perf_counter()
    p = subprocess.run(native_cmd(wl, k), capture_output=True, text=True, timeout=TIMEOUT)
    dt = time.perf_counter() - t0
    if p.returncode != 0:
        raise RuntimeError(f"native rc={p.returncode}: {p.stderr.strip()[-400:]}")
    internal = None
    for line in p.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and "internal_ms" in line:
            try:
                internal = json.loads(line).get("internal_ms")
            except json.JSONDecodeError:
                pass
    return dt, internal


def run_wasm(cfg, wl, k):
    stdin = wl["setup"] + "\n" + (wl["query"] + "\n") * k
    t0 = time.perf_counter()
    p = subprocess.run(wasm_cmd(cfg, wl), input=stdin, capture_output=True, text=True, timeout=TIMEOUT)
    dt = time.perf_counter() - t0
    if p.returncode != 0:
        raise RuntimeError(f"wasm rc={p.returncode}: {p.stderr.strip()[-400:]}")
    return dt, None


def runner_for(cfg):
    # measure() calls runner(wl, k); both underlying runners also need cfg.
    if cfg["kind"] == "native":
        return lambda wl, k: run_native(cfg, wl, k)
    return lambda wl, k: run_wasm(cfg, wl, k)


# --- stats (identical method to run.py) ------------------------------------

def linfit(xs, ys):
    n = len(xs)
    mx = sum(xs) / n
    my = sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    slope = sxy / sxx if sxx else 0.0
    intercept = my - slope * mx
    sst = sum((y - my) ** 2 for y in ys)
    ssr = sum((y - (intercept + slope * x)) ** 2 for x, y in zip(xs, ys))
    r2 = 1 - ssr / sst if sst else 1.0
    return slope, intercept, r2


def measure(wl, runner, reps):
    ks = wl["k_values"]
    samples = {k: [] for k in ks}
    internals = {k: [] for k in ks}
    for _ in range(reps):
        for k in ks:
            dt, internal = runner(wl, k)
            samples[k].append(dt)
            if internal is not None:
                internals[k].append(internal)
    med = {k: statistics.median(v) for k, v in samples.items()}
    slope_s, intercept_s, r2 = linfit(ks, [med[k] for k in ks])
    rep_slopes = []
    for r in range(reps):
        ys = [samples[k][r] for k in ks]
        s, _, _ = linfit(ks, ys)
        rep_slopes.append(s * 1000.0)
    rep_slopes.sort()
    out = {
        "slope_ms_per_query": slope_s * 1000.0,
        "fixed_cost_ms": intercept_s * 1000.0,
        "r2": r2,
        "slope_ms_median": statistics.median(rep_slopes),
        "slope_ms_p25": rep_slopes[len(rep_slopes) // 4] if rep_slopes else None,
        "slope_ms_p75": rep_slopes[(3 * len(rep_slopes)) // 4] if rep_slopes else None,
        "median_wall_ms": {str(k): med[k] * 1000.0 for k in ks},
    }
    kmax = max(ks)
    if internals.get(kmax):
        out["internal_ms_per_query_at_kmax"] = statistics.median(internals[kmax]) / kmax if kmax else None
    return out


# --- verification ----------------------------------------------------------

def verify_cell(cfg, wl):
    """Return None if the (config, workload) cell resolves, else a reason."""
    if cfg["kind"] == "wasm":
        if core_for(cfg, wl) is None:
            return "artifacts absent (no core)"
        if cli_for(cfg) is None:
            return "artifacts absent (no cli)"
    try:
        if cfg["kind"] == "native":
            p = subprocess.run(native_cmd(wl, 1), capture_output=True, text=True, timeout=TIMEOUT)
            if p.returncode != 0:
                return f"native rc={p.returncode}: {p.stderr.strip()[-160:]}"
        else:
            stdin = wl["setup"] + "\n" + wl["query"] + "\n"
            p = subprocess.run(wasm_cmd(cfg, wl), input=stdin, capture_output=True, text=True, timeout=TIMEOUT)
            blob = p.stdout + p.stderr
            for m in ERROR_MARKERS:
                if m in blob:
                    return f"wasm: '{m}'"
            # Ext-scoped load failure (ignore harmless built-in autoload noise).
            ext = wl["extension"]
            for m in (f"host declined extension '{ext}'",
                      f"failed to load extension {ext}",
                      f"extension instantiation for {ext} failed"):
                if m in blob:
                    return f"wasm: '{m}'"
    except subprocess.TimeoutExpired:
        return "timeout"
    return None


# --- main ------------------------------------------------------------------

def platform_info():
    info = {"platform": sys.platform}
    for key, args in (("cpu", ["sysctl", "-n", "machdep.cpu.brand_string"]),
                      ("wasmtime", ["wasmtime", "--version"])):
        try:
            info[key] = subprocess.check_output(args, text=True).strip()
        except Exception:
            pass
    return info


def geomean(xs):
    xs = [x for x in xs if x == x and x > 0]
    return math.exp(sum(math.log(x) for x in xs) / len(xs)) if xs else float("nan")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--only", help="comma-separated workload ids")
    ap.add_argument("--configs", help="comma-separated config ids")
    ap.add_argument("--verify-only", action="store_true")
    ap.add_argument("--reuse", action="store_true",
                    help="reuse 'ok' cells from the existing results JSON (only "
                         "re-measure errored/missing cells) -- avoids re-running "
                         "the slow configs when patching a partial matrix")
    args = ap.parse_args()

    spec = json.loads((HERE / "workloads.json").read_text())
    mat = json.loads((HERE / "matrix.json").read_text())
    label = os.environ.get("CONTRACT_LABEL", mat.get("contract_label", "unknown"))
    workloads = spec["workloads"]
    configs = mat["configs"]
    if args.only:
        want = set(args.only.split(","))
        workloads = [w for w in workloads if w["id"] in want]
    if args.configs:
        want = set(args.configs.split(","))
        configs = [c for c in configs if c["id"] in want]

    print(f"native-vs-wasm MATRIX @ {label}")
    print(f"  HOST_BIN   = {HOST_BIN}")
    print(f"  NATIVE_BIN = {NATIVE_BIN}")
    print(f"  NVW_ART    = {ART}")
    print(f"  NVW_AOT    = {AOT}")
    print(f"  NVW_EMBED  = {EMBED}\n")

    # --reuse: load prior 'ok' cells so we only re-measure what changed.
    prior = {}
    res_path = HERE / "results" / f"matrix-{label}.json"
    if args.reuse and res_path.exists():
        old = json.loads(res_path.read_text())
        for r in old.get("workloads", []):
            for cid, c in r.get("cells", {}).items():
                if c.get("status") == "ok":
                    prior[(cid, r["id"])] = c
        print(f"--reuse: {len(prior)} prior 'ok' cells loaded from {res_path.name}\n")

    # Verification pass: which cells resolve.
    print("verifying cells resolve...")
    resolvable = {}  # (cfg_id, wl_id) -> reason or None
    for cfg in configs:
        for wl in workloads:
            key = (cfg["id"], wl["id"])
            if key in prior:
                resolvable[key] = None
                print(f"  [REUSE] {cfg['id']} x {wl['id']}")
                continue
            reason = verify_cell(cfg, wl)
            resolvable[key] = reason
            tag = "OK" if reason is None else f"SKIP ({reason})"
            print(f"  [{tag}] {cfg['id']} x {wl['id']}")
    if args.verify_only:
        return

    # Measurement.
    cells = {}  # (cfg_id, wl_id) -> result dict
    for cfg in configs:
        runner = runner_for(cfg)
        for wl in workloads:
            key = (cfg["id"], wl["id"])
            if key in prior:
                cells[key] = prior[key]
                print(f"reusing {cfg['id']} x {wl['id']} "
                      f"(slope {prior[key]['slope_ms_per_query']:.3f} ms/q)")
                continue
            if resolvable[key] is not None:
                cells[key] = {"status": "skipped", "reason": resolvable[key]}
                continue
            reps = 2 if args.quick else wl["reps"]
            print(f"measuring {cfg['id']} x {wl['id']} (reps={reps}) ...")
            try:
                r = measure(wl, runner, reps)
                r["status"] = "ok"
                r["rows_per_s"] = wl["rows"] / (r["slope_ms_per_query"] / 1000.0) if r["slope_ms_per_query"] else 0
                cells[key] = r
                print(f"    slope {r['slope_ms_per_query']:.3f} ms/q  "
                      f"fixed {r['fixed_cost_ms']:.0f} ms  r2 {r['r2']:.4f}")
            except Exception as e:
                cells[key] = {"status": "error", "reason": str(e)[-300:]}
                print(f"    ERROR: {str(e)[-200:]}")

    out = build_output(label, configs, workloads, cells)
    res_path = HERE / "results" / f"matrix-{label}.json"
    res_path.write_text(json.dumps(out, indent=2))
    write_report(out, label)
    print(f"\nwrote {res_path}")
    print(f"wrote {HERE / 'MATRIX.md'}")


def build_output(label, configs, workloads, cells):
    def cell(cfg_id, wl_id):
        return cells.get((cfg_id, wl_id), {"status": "skipped", "reason": "not run"})

    wl_records = []
    for wl in workloads:
        rec = {"id": wl["id"], "category": wl["category"], "extension": wl["extension"],
               "rows": wl["rows"], "cells": {}}
        nat = cell("native", wl["id"])
        nat_slope = nat.get("slope_ms_per_query") if nat.get("status") == "ok" else None
        for cfg in configs:
            c = cell(cfg["id"], wl["id"])
            if c.get("status") == "ok":
                entry = {
                    "status": "ok",
                    "slope_ms_per_query": c["slope_ms_per_query"],
                    "fixed_cost_ms": c["fixed_cost_ms"],
                    "r2": c["r2"],
                    "rows_per_s": c["rows_per_s"],
                    "slope_ms_p25": c.get("slope_ms_p25"),
                    "slope_ms_p75": c.get("slope_ms_p75"),
                }
                if nat_slope and c["slope_ms_per_query"]:
                    entry["overhead_vs_native_pct"] = (c["slope_ms_per_query"] / nat_slope - 1.0) * 100.0
                if "internal_ms_per_query_at_kmax" in c:
                    entry["internal_ms_per_query_at_kmax"] = c["internal_ms_per_query_at_kmax"]
            else:
                entry = {"status": c.get("status", "skipped"), "reason": c.get("reason", "")}
            rec["cells"][cfg["id"]] = entry
        wl_records.append(rec)

    # --- key deltas ---
    deltas = compute_deltas(configs, wl_records)

    return {
        "contract_label": label,
        "host": platform_info(),
        "configs": [{"id": c["id"], "title": c.get("title", c["id"]),
                     "kind": c["kind"], "aot": c.get("aot", False),
                     "embedded": c.get("embedded", False),
                     "note": c.get("note", "")} for c in configs],
        "workloads": wl_records,
        "deltas": deltas,
    }


def compute_deltas(configs, wl_records):
    cfg_ids = [c["id"] for c in configs]

    def ok_cells(cid):
        return {r["id"]: r["cells"][cid] for r in wl_records
                if r["cells"].get(cid, {}).get("status") == "ok"}

    deltas = {}

    # (1) each-vs-native: geomean overhead per config.
    per_config = {}
    for cid in cfg_ids:
        ovs = [r["cells"][cid].get("overhead_vs_native_pct")
               for r in wl_records if r["cells"].get(cid, {}).get("status") == "ok"
               and r["cells"][cid].get("overhead_vs_native_pct") is not None]
        ratios = [1.0 + o / 100.0 for o in ovs]
        if ratios:
            g = geomean(ratios)
            per_config[cid] = {"geomean_ratio_vs_native": g,
                               "geomean_overhead_pct": (g - 1.0) * 100.0,
                               "n": len(ratios)}
    deltas["each_vs_native"] = per_config

    # (2) dynamic vs embedded (slope) -- per workload + geomean, both AOT tiers.
    dyn_emb = {}
    for aot, dyn_id, emb_id in (("noaot", "wasm-dynamic-noaot", "wasm-embedded-noaot"),
                                ("aot", "wasm-dynamic-aot", "wasm-embedded-aot")):
        d, e = ok_cells(dyn_id), ok_cells(emb_id)
        common = sorted(set(d) & set(e))
        rows = {}
        ratios = []
        for w in common:
            ds, es = d[w]["slope_ms_per_query"], e[w]["slope_ms_per_query"]
            if ds and es:
                speedup = ds / es  # how much faster embedded is
                rows[w] = {"dynamic_ms": ds, "embedded_ms": es,
                           "dynamic_over_embedded": speedup,
                           "dispatch_boundary_pct_of_dynamic": (ds - es) / ds * 100.0}
                ratios.append(speedup)
        dyn_emb[aot] = {"per_workload": rows,
                        "geomean_dynamic_over_embedded": geomean(ratios) if ratios else None}
    deltas["dynamic_vs_embedded"] = dyn_emb

    # (3) AOT vs no-AOT: fixed-cost reduction + slope parity, dynamic + embedded.
    aot_noaot = {}
    for tier, noaot_id, aot_id in (("dynamic", "wasm-dynamic-noaot", "wasm-dynamic-aot"),
                                   ("embedded", "wasm-embedded-noaot", "wasm-embedded-aot")):
        n, a = ok_cells(noaot_id), ok_cells(aot_id)
        common = sorted(set(n) & set(a))
        rows = {}
        slope_ratios = []
        fixed_noaot = []
        fixed_aot = []
        for w in common:
            ns, as_ = n[w]["slope_ms_per_query"], a[w]["slope_ms_per_query"]
            nf, af = n[w]["fixed_cost_ms"], a[w]["fixed_cost_ms"]
            rows[w] = {"slope_noaot_ms": ns, "slope_aot_ms": as_,
                       "slope_ratio_aot_over_noaot": (as_ / ns) if ns else None,
                       "fixed_noaot_ms": nf, "fixed_aot_ms": af,
                       "fixed_reduction_ms": nf - af}
            if ns:
                slope_ratios.append(as_ / ns)
            fixed_noaot.append(nf)
            fixed_aot.append(af)
        aot_noaot[tier] = {
            "per_workload": rows,
            "geomean_slope_ratio_aot_over_noaot": geomean(slope_ratios) if slope_ratios else None,
            "median_fixed_noaot_ms": statistics.median(fixed_noaot) if fixed_noaot else None,
            "median_fixed_aot_ms": statistics.median(fixed_aot) if fixed_aot else None,
        }
    deltas["aot_vs_noaot"] = aot_noaot

    return deltas


def write_report(out, label):
    cfgs = out["configs"]
    cfg_ids = [c["id"] for c in cfgs]
    L = []
    L.append(f"# Native-vs-WASM configuration matrix @ {label}\n")
    h = out["host"]
    L.append(f"Host: {h.get('cpu','?')} | {h.get('wasmtime','?')} | {h.get('platform','?')}\n")
    L.append("Cells are the marginal per-query cost (regression slope over query "
             "count; process spawn, wasm compile, extension load and table setup "
             "are the intercept and are excluded). `fixed` is that intercept (the "
             "one-time/startup cost), reported separately. A cell with r2 < 0.99 "
             "is flagged.\n")

    # Config legend.
    L.append("## Configs\n")
    for c in cfgs:
        L.append(f"- **{c['id']}** — {c['title']}. {c['note']}")
    L.append("")

    # Throughput matrix (slope ms/q).
    L.append("## Throughput — marginal per-query cost (ms/q, lower better)\n")
    L.append("| workload | rows | " + " | ".join(cfg_ids) + " |")
    L.append("|---|--:|" + "|".join(["--:"] * len(cfg_ids)) + "|")
    for r in out["workloads"]:
        cells = []
        for cid in cfg_ids:
            c = r["cells"][cid]
            if c.get("status") == "ok":
                flag = "" if c["r2"] >= 0.99 else "*"
                cells.append(f"{c['slope_ms_per_query']:.2f}{flag}")
            else:
                cells.append(f"_{c.get('status','-')}_")
        L.append(f"| {r['id']} | {r['rows']:,} | " + " | ".join(cells) + " |")
    L.append("")

    # Overhead-vs-native matrix.
    L.append("## Overhead vs native (%, lower better)\n")
    L.append("| workload | " + " | ".join(cfg_ids) + " |")
    L.append("|---|" + "|".join(["--:"] * len(cfg_ids)) + "|")
    for r in out["workloads"]:
        cells = []
        for cid in cfg_ids:
            c = r["cells"][cid]
            if c.get("status") == "ok" and c.get("overhead_vs_native_pct") is not None:
                cells.append(f"{c['overhead_vs_native_pct']:+.0f}%")
            elif cid == "native" and c.get("status") == "ok":
                cells.append("(baseline)")
            else:
                cells.append("-")
        L.append(f"| {r['id']} | " + " | ".join(cells) + " |")
    L.append("")

    # Fixed (startup) matrix.
    L.append("## Fixed cost — one-time startup (ms; spawn + compile/deserialize + load + setup)\n")
    L.append("| workload | " + " | ".join(cfg_ids) + " |")
    L.append("|---|" + "|".join(["--:"] * len(cfg_ids)) + "|")
    for r in out["workloads"]:
        cells = []
        for cid in cfg_ids:
            c = r["cells"][cid]
            cells.append(f"{c['fixed_cost_ms']:.0f}" if c.get("status") == "ok" else "-")
        L.append(f"| {r['id']} | " + " | ".join(cells) + " |")
    L.append("")

    # Key deltas.
    d = out["deltas"]
    L.append("## Key deltas\n")

    L.append("### 1. Each config vs native (geomean overhead across workloads)\n")
    L.append("| config | geomean ratio vs native | overhead | n |")
    L.append("|---|--:|--:|--:|")
    for cid in cfg_ids:
        e = d["each_vs_native"].get(cid)
        if e:
            L.append(f"| {cid} | {e['geomean_ratio_vs_native']:.2f}x | "
                     f"{e['geomean_overhead_pct']:+.1f}% | {e['n']} |")
    L.append("")

    L.append("### 2. Dynamic vs embedded — the dynamic-loading dispatch-boundary cost\n")
    for tier in ("noaot", "aot"):
        de = d["dynamic_vs_embedded"].get(tier, {})
        g = de.get("geomean_dynamic_over_embedded")
        if g is None and not de.get("per_workload"):
            L.append(f"- **{tier}**: not measurable (embedded config unavailable in this run).")
            continue
        if g:
            L.append(f"- **{tier}**: embedded is {g:.2f}x faster than dynamic "
                     f"(geomean); i.e. the host-mediated dispatch boundary is "
                     f"~{(1-1/g)*100:.0f}% of the dynamic per-query cost.")
        for w, row in de.get("per_workload", {}).items():
            L.append(f"  - {w}: dynamic {row['dynamic_ms']:.2f} ms/q vs embedded "
                     f"{row['embedded_ms']:.2f} ms/q "
                     f"({row['dispatch_boundary_pct_of_dynamic']:.0f}% boundary).")
    L.append("")

    L.append("### 3. AOT vs no-AOT — compile/startup cost (slope must be unchanged)\n")
    for tier in ("dynamic", "embedded"):
        a = d["aot_vs_noaot"].get(tier, {})
        if not a.get("per_workload"):
            L.append(f"- **{tier}**: not measurable (one AOT tier unavailable).")
            continue
        sr = a.get("geomean_slope_ratio_aot_over_noaot")
        fn, fa = a.get("median_fixed_noaot_ms"), a.get("median_fixed_aot_ms")
        srtxt = f"{sr:.3f}x" if sr is not None else "n/a"
        L.append(f"- **{tier}**: median fixed cost {fn:.0f} ms (no-AOT) -> "
                 f"{fa:.0f} ms (AOT) = {fn-fa:.0f} ms saved per startup; "
                 f"steady-state slope ratio AOT/no-AOT = {srtxt} (≈1.0 confirms "
                 f"AOT changes startup, not throughput).")
    L.append("")

    # Skipped/blocked configs -- data-driven summary of why a column is empty.
    skipped = {}
    for r in out["workloads"]:
        for cid in cfg_ids:
            c = r["cells"][cid]
            if c.get("status") not in (None, "ok"):
                skipped.setdefault(cid, c.get("reason", c.get("status", "skipped")))
    if skipped:
        L.append("## Skipped configs (this run)\n")
        for cid, reason in skipped.items():
            L.append(f"- **{cid}**: {reason}")
        L.append("\nSee `README.md` (\"Status of the embedded configs\" / AOT) for "
                 "why these are not built here and the expected magnitudes; the "
                 "harness measures them with no code change once `NVW_AOT` / "
                 "`NVW_EMBED` artifacts exist.")
        L.append("")

    # Interpretation -- where the measured wasm overhead lives.
    if "wasm-dynamic-noaot" in d.get("each_vs_native", {}):
        g = d["each_vs_native"]["wasm-dynamic-noaot"]["geomean_ratio_vs_native"]
        L.append("## Interpretation\n")
        L.append(f"The shipped wasm path (dynamic, no-AOT) is **{g:.2f}x native** "
                 "(geomean) on steady-state throughput, with the gap widest on the "
                 "dispatch-/compute-bound scalars and narrowest on the aggregate. "
                 "Because the slope method excludes one-time compile (it lands in "
                 "`fixed`), this gap is the **per-row engine + dispatch** cost, not "
                 "compile: it is the wasm DuckDB core's per-row scan plus the "
                 "host-mediated cross-component WIT marshalling. AOT (config 3) "
                 "targets only the `fixed` column (the one-time core compile) and "
                 "by construction leaves the slope unchanged; embedding (configs "
                 "4-5) is the lever that attacks this slope by removing the dispatch "
                 "boundary. The @4.0.0 columnar ABI is expected to cut the same "
                 "slope further.")
        L.append("")

    (HERE / "MATRIX.md").write_text("\n".join(L) + "\n")


if __name__ == "__main__":
    main()
