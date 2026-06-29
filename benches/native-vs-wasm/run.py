#!/usr/bin/env python3
"""Head-to-head NATIVE-vs-WASM benchmark for ducklink extensions.

The SAME wasm `duckdb:extension` component + SAME SQL + SAME data, run two ways:

  * NATIVE  -- native (bundled) DuckDB + the native `ducklink` extension
              dispatching to the component (the `nvw-native-runner` binary).
  * WASM    -- the wasm DuckDB core run under wasmtime/wasi via the `ducklink`
              host CLI, loading the same component.

METHODOLOGY (see METHODOLOGY.md). For each (path, workload) we run the query
`K` times back-to-back inside ONE process and time the WHOLE process externally,
for several K (the first K MUST be 0). We then fit

        wall_time(K) = intercept + slope * K

by least squares. `slope` is the marginal per-query cost; `intercept` is the
one-time cost (process spawn, wasm Cranelift compile, extension/component load,
table setup) -- which is exactly what we want to EXCLUDE, and which we ALSO
report separately. The fit r^2 is the defensibility gate: we flag any workload
whose r^2 < 0.99. The native runner additionally prints its internally-timed
K-loop (`internal_ms`) as an independent cross-check of the external slope.

The native-vs-wasm overhead per workload is (wasm_slope / native_slope - 1).
The honest aggregate is the GEOMETRIC mean of the per-workload ratios (geomean
does not let one outlier dominate and is the right average for ratios).

Usage:
    ./run.py                 # full run -> results/ + REPORT.md
    ./run.py --quick         # fewer reps (smoke)
    ./run.py --only aba_validate_1m,fnv1a_64_1m
    ./run.py --verify-only   # just check every workload resolves on both paths

Config via env (defaults target the sibling main checkout that holds the
prebuilt @3.1.0 artifacts; point these at a @4.0.0 build to re-run):
    DUCKLINK_REPO   repo holding prebuilt artifacts   (default ~/git/ducklink)
    HOST_BIN        ducklink host binary              ($DUCKLINK_REPO/target/release/ducklink)
    EXT_DIR         component dir                     ($DUCKLINK_REPO/artifacts/extensions)
    NATIVE_BIN      nvw-native-runner binary          (./native-runner/target/release/nvw-native-runner)
    CONTRACT_LABEL  label for the report              (workloads.json contract_label)
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
DEFAULT_REPO = Path(os.environ.get("DUCKLINK_REPO", str(Path.home() / "git" / "ducklink")))
HOST_BIN = Path(os.environ.get("HOST_BIN", str(DEFAULT_REPO / "target" / "release" / "ducklink")))
EXT_DIR = Path(os.environ.get("EXT_DIR", str(DEFAULT_REPO / "artifacts" / "extensions")))
NATIVE_BIN = Path(os.environ.get(
    "NATIVE_BIN",
    str(HERE / ".." / ".." / "native-extension" / "ducklink" / "target" / "release" / "nvw_native_runner")))
TIMEOUT = int(os.environ.get("NVW_TIMEOUT", "600"))

ERROR_MARKERS = [
    "panicked", "Catalog Error", "Binder Error", "Parser Error",
    "did not find function", "Function with name", "no artifact found",
    "failed to preload", "extension instantiation",
]


def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def check_env():
    missing = [str(p) for p in (HOST_BIN, EXT_DIR, NATIVE_BIN) if not p.exists()]
    if missing:
        die("missing prerequisites:\n  " + "\n  ".join(missing) +
            "\n(build the host with `make host`, build native-runner with "
            "`cargo build --release` in native-runner/, and set DUCKLINK_REPO)")


# --- runners ---------------------------------------------------------------

def time_native(wl, k):
    """External wall time (seconds) of the native runner doing K queries."""
    cmd = [str(NATIVE_BIN),
           "--component", str(EXT_DIR / wl["component"]),
           "--name", wl["extension"],
           "--setup", wl["setup"],
           "--query", wl["query"],
           "--iters", str(k)]
    if wl.get("needs_raw"):
        cmd.append("--raw")
    t0 = time.perf_counter()
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=TIMEOUT)
    dt = time.perf_counter() - t0
    if p.returncode != 0:
        raise RuntimeError(f"native runner failed (k={k}): {p.stderr.strip()[-500:]}")
    internal = None
    for line in p.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and "internal_ms" in line:
            try:
                internal = json.loads(line).get("internal_ms")
            except json.JSONDecodeError:
                pass
    return dt, internal


def time_wasm(wl, k):
    """External wall time (seconds) of the wasm core CLI doing K queries."""
    stdin = wl["setup"] + "\n" + (wl["query"] + "\n") * k
    cmd = [str(HOST_BIN), "--extensions-dir", str(EXT_DIR),
           "--", ":memory:", "--load-extension", wl["extension"]]
    t0 = time.perf_counter()
    p = subprocess.run(cmd, input=stdin, capture_output=True, text=True, timeout=TIMEOUT)
    dt = time.perf_counter() - t0
    if p.returncode != 0:
        raise RuntimeError(f"wasm CLI failed (k={k}): {p.stderr.strip()[-500:]}")
    return dt, None


# --- stats -----------------------------------------------------------------

def linfit(xs, ys):
    """Least-squares slope, intercept, r^2."""
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
    """Run `runner` over k_values x reps; return per-K samples + the pooled fit
    (slope/intercept on median-per-K) + per-rep slopes for variance."""
    ks = wl["k_values"]
    samples = {k: [] for k in ks}        # seconds, external wall
    internals = {k: [] for k in ks}      # native cross-check (ms)
    for _ in range(reps):
        for k in ks:
            dt, internal = runner(wl, k)
            samples[k].append(dt)
            if internal is not None:
                internals[k].append(internal)
    med = {k: statistics.median(v) for k, v in samples.items()}
    slope_s, intercept_s, r2 = linfit(ks, [med[k] for k in ks])

    # Per-rep slopes (fit each rep across K) -> slope distribution / variance.
    rep_slopes = []
    for r in range(reps):
        ys = [samples[k][r] for k in ks]
        s, _, _ = linfit(ks, ys)
        rep_slopes.append(s * 1000.0)  # ms/query
    rep_slopes.sort()

    out = {
        "k_values": ks,
        "median_wall_ms": {str(k): med[k] * 1000.0 for k in ks},
        "slope_ms_per_query": slope_s * 1000.0,
        "fixed_cost_ms": intercept_s * 1000.0,
        "r2": r2,
        "rep_slopes_ms": rep_slopes,
        "slope_ms_median": statistics.median(rep_slopes),
        "slope_ms_p25": rep_slopes[len(rep_slopes) // 4] if rep_slopes else None,
        "slope_ms_p75": rep_slopes[(3 * len(rep_slopes)) // 4] if rep_slopes else None,
    }
    # Native internal cross-check at max K.
    kmax = max(ks)
    if internals.get(kmax):
        out["internal_ms_per_query_at_kmax"] = statistics.median(internals[kmax]) / kmax if kmax else None
    return out


# --- verification ----------------------------------------------------------

def verify(wl):
    """Run each workload once on both paths capturing output; flag errors."""
    problems = []
    # wasm
    stdin = wl["setup"] + "\n" + wl["query"] + "\n"
    cmd = [str(HOST_BIN), "--extensions-dir", str(EXT_DIR),
           "--", ":memory:", "--load-extension", wl["extension"]]
    p = subprocess.run(cmd, input=stdin, capture_output=True, text=True, timeout=TIMEOUT)
    blob = (p.stdout + p.stderr)
    for m in ERROR_MARKERS:
        if m in blob:
            problems.append(f"wasm: '{m}'")
            break
    # native
    cmd = [str(NATIVE_BIN), "--component", str(EXT_DIR / wl["component"]),
           "--name", wl["extension"], "--setup", wl["setup"],
           "--query", wl["query"], "--iters", "1"]
    if wl.get("needs_raw"):
        cmd.append("--raw")
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=TIMEOUT)
    if p.returncode != 0:
        problems.append(f"native: rc={p.returncode} {p.stderr.strip()[-200:]}")
    return problems


# --- main ------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true", help="fewer reps (smoke)")
    ap.add_argument("--only", help="comma-separated workload ids")
    ap.add_argument("--verify-only", action="store_true")
    args = ap.parse_args()

    check_env()
    spec = json.loads((HERE / "workloads.json").read_text())
    label = os.environ.get("CONTRACT_LABEL", spec.get("contract_label", "unknown"))
    workloads = spec["workloads"]
    if args.only:
        want = set(args.only.split(","))
        workloads = [w for w in workloads if w["id"] in want]

    print(f"native-vs-wasm @ {label}")
    print(f"  HOST_BIN  = {HOST_BIN}")
    print(f"  EXT_DIR   = {EXT_DIR}")
    print(f"  NATIVE_BIN= {NATIVE_BIN}\n")

    # Verify everything resolves before spending time measuring.
    print("verifying workloads resolve on both paths...")
    usable = []
    for wl in workloads:
        probs = verify(wl)
        status = "OK" if not probs else "SKIP (" + "; ".join(probs) + ")"
        print(f"  [{status}] {wl['id']}")
        if not probs:
            usable.append(wl)
    if args.verify_only:
        return
    if not usable:
        die("no workloads usable")

    results = []
    for wl in usable:
        reps = 2 if args.quick else wl["reps"]
        print(f"\nmeasuring {wl['id']} ({wl['category']}, reps={reps}) ...")
        nat = measure(wl, time_native, reps)
        was = measure(wl, time_wasm, reps)
        ratio = was["slope_ms_per_query"] / nat["slope_ms_per_query"] if nat["slope_ms_per_query"] else float("nan")
        overhead = (ratio - 1.0) * 100.0
        rows = wl["rows"]
        nat_tput = rows / (nat["slope_ms_per_query"] / 1000.0) if nat["slope_ms_per_query"] else 0
        was_tput = rows / (was["slope_ms_per_query"] / 1000.0) if was["slope_ms_per_query"] else 0
        rec = {
            "id": wl["id"], "category": wl["category"], "extension": wl["extension"],
            "rows": rows, "native": nat, "wasm": was,
            "ratio_wasm_over_native": ratio, "overhead_pct": overhead,
            "native_rows_per_s": nat_tput, "wasm_rows_per_s": was_tput,
            "min_r2": min(nat["r2"], was["r2"]),
        }
        results.append(rec)
        print(f"    native {nat['slope_ms_per_query']:.3f} ms/q (r2={nat['r2']:.4f})  "
              f"wasm {was['slope_ms_per_query']:.3f} ms/q (r2={was['r2']:.4f})  "
              f"overhead {overhead:+.1f}%")

    # Honest aggregate: geometric mean of per-workload ratios.
    ratios = [r["ratio_wasm_over_native"] for r in results if r["ratio_wasm_over_native"] == r["ratio_wasm_over_native"]]
    geo = math.exp(sum(math.log(x) for x in ratios) / len(ratios)) if ratios else float("nan")

    out = {
        "contract_label": label,
        "host": platform_info(),
        "aggregate": {
            "geomean_ratio_wasm_over_native": geo,
            "geomean_overhead_pct": (geo - 1.0) * 100.0,
            "n_workloads": len(results),
        },
        "workloads": results,
    }
    res_path = HERE / "results" / f"baseline-{label}.json"
    res_path.write_text(json.dumps(out, indent=2))
    write_report(out, label)
    print(f"\naggregate (geomean): wasm is {geo:.2f}x native "
          f"({(geo-1)*100:+.1f}% overhead) across {len(results)} workloads")
    print(f"wrote {res_path}")
    print(f"wrote {HERE / 'REPORT.md'}")


def platform_info():
    info = {"platform": sys.platform}
    try:
        info["cpu"] = subprocess.check_output(
            ["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip()
    except Exception:
        pass
    try:
        info["wasmtime"] = subprocess.check_output(["wasmtime", "--version"], text=True).strip()
    except Exception:
        pass
    return info


def write_report(out, label):
    L = []
    L.append(f"# Native-vs-WASM baseline @ {label}\n")
    h = out["host"]
    L.append(f"Host: {h.get('cpu','?')} | {h.get('wasmtime','?')} | {h.get('platform','?')}\n")
    L.append("Overhead = wasm marginal per-query cost / native marginal per-query "
             "cost - 1. Marginal cost is the regression slope over query count, so "
             "process spawn, wasm compile, extension load and table setup are "
             "excluded (they are the intercept, reported as `fixed`). Lower is "
             "better; a workload with `r2 < 0.99` is flagged as not defensible.\n")
    L.append("| workload | category | native ms/q | wasm ms/q | overhead | native rows/s | wasm rows/s | min r2 |")
    L.append("|---|---|--:|--:|--:|--:|--:|--:|")
    for r in out["workloads"]:
        flag = "" if r["min_r2"] >= 0.99 else " ⚠"
        L.append(f"| {r['id']} | {r['category']} | "
                 f"{r['native']['slope_ms_per_query']:.3f} | "
                 f"{r['wasm']['slope_ms_per_query']:.3f} | "
                 f"{r['overhead_pct']:+.1f}%{flag} | "
                 f"{r['native_rows_per_s']/1e6:.1f}M | "
                 f"{r['wasm_rows_per_s']/1e6:.1f}M | {r['min_r2']:.4f} |")
    agg = out["aggregate"]
    L.append(f"\n**Aggregate (geomean of ratios, n={agg['n_workloads']}): "
             f"wasm is {agg['geomean_ratio_wasm_over_native']:.2f}x native "
             f"({agg['geomean_overhead_pct']:+.1f}% overhead).**\n")
    L.append("Per-workload detail (fixed = one-time cost excluded from the headline):\n")
    for r in out["workloads"]:
        n, w = r["native"], r["wasm"]
        L.append(f"- **{r['id']}** ({r['rows']:,} rows): "
                 f"native slope {n['slope_ms_per_query']:.3f} ms/q "
                 f"(fixed {n['fixed_cost_ms']:.0f} ms, r2 {n['r2']:.4f}); "
                 f"wasm slope {w['slope_ms_per_query']:.3f} ms/q "
                 f"(fixed {w['fixed_cost_ms']:.0f} ms, r2 {w['r2']:.4f}).")
        if "internal_ms_per_query_at_kmax" in n and n["internal_ms_per_query_at_kmax"]:
            L.append(f"  - native cross-check (internal timer): "
                     f"{n['internal_ms_per_query_at_kmax']:.3f} ms/q vs slope "
                     f"{n['slope_ms_per_query']:.3f} ms/q.")
    (HERE / "REPORT.md").write_text("\n".join(L) + "\n")


if __name__ == "__main__":
    main()
