#!/usr/bin/env python3
"""Aggregate every analysis producer into a single results/results.json.

This is the one machine-readable source for the headline numbers. The Rust
analysis binary writes numbers.tex, stats.txt and the summary_*.csv tables; the
RCAEval benchmark scripts write rcaeval_comparison.csv; the energy harness writes
energy.txt. Each number is tagged with the configuration it was measured under
(for example the service count and sample rate for a byte figure) so that two
figures that differ only by scale no longer read as a contradiction.

Run after `make analysis` (and after the bench and energy steps when present):
    python3 scripts/analysis/build_results_json.py
"""
import csv
import json
import re
import statistics as st
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
A = ROOT / "results" / "analysis"
P = ROOT / "results"


def read_text(path):
    try:
        return path.read_text()
    except FileNotFoundError:
        return ""


def parse_macros(path):
    """numbers.tex -> {macroName: value_string}."""
    out = {}
    # One macro per line; greedy to the trailing brace so values that themselves
    # contain braces (for example a p-value rendered as "{<}0.001") survive.
    for line in read_text(path).splitlines():
        m = re.match(r"\s*\\newcommand\{\\([A-Za-z]+)\}\{(.*)\}\s*$", line)
        if m:
            out[m.group(1)] = m.group(2)
    return out


def fnum(s):
    """Best-effort float from a macro/cell ('{<}0.001', '+6.4', '2.2x' -> float)."""
    if s is None:
        return None
    s = str(s).strip().replace("{<}", "").replace("<", "").replace("x", "")
    s = s.replace("+", "").replace("%", "")
    try:
        return float(s)
    except ValueError:
        return None


def read_csv(path):
    if not path.exists():
        return []
    with path.open() as f:
        return list(csv.DictReader(f))


def parse_energy(path):
    out = {}
    for line in read_text(path).splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            out[k.strip()] = fnum(v) if fnum(v) is not None else v.strip()
    return out


def parse_stats_bytes(path):
    """Pull the base-size byte line from stats.txt (absolute byte counts)."""
    txt = read_text(path)
    m = re.search(
        r"base size, 10 Hz\): central-sf ([\d.]+), edge-local ([\d.]+), hybrid ([\d.]+)",
        txt,
    )
    if not m:
        return None
    return {
        "central_sf_bytes": float(m.group(1)),
        "edge_local_bytes": float(m.group(2)),
        "hybrid_bytes": float(m.group(3)),
    }


def summarize_capped(path):
    """Collector footprint measured under a cgroup v2 cap (B7).

    One row per partition event; cross_bytes/cpu_ms/rss_kb/p99_handler_us are read
    from the cloud collector's /status, differenced per event for the counters.
    The collector binary and the 103-service workload are identical across the four
    production tiers, so this is the footprint of the pipeline under the cap, not a
    property of one orchestrator.
    """
    rows = read_csv(path)
    rows = [r for r in rows if fnum(r.get("cross_bytes")) is not None]
    if not rows:
        return None

    def col(name):
        return [v for v in (fnum(r.get(name)) for r in rows) if v is not None]

    def med(xs):
        return st.median(xs) if xs else None

    def mx(xs):
        return max(xs) if xs else None

    cb, cpu, rss, p99 = col("cross_bytes"), col("cpu_ms"), col("rss_kb"), col("p99_handler_us")
    return {
        "events": len(rows),
        "cap_cpus": fnum(rows[0].get("cap_cpus")),
        "cap_mem_mb": fnum(rows[0].get("cap_mem_mb")),
        "cross_bytes_median": med(cb), "cross_bytes_max": mx(cb),
        "cpu_ms_median": med(cpu), "cpu_ms_max": mx(cpu),
        "rss_kb_median": med(rss), "rss_kb_max": mx(rss),
        "p99_handler_us_median": med(p99), "p99_handler_us_max": mx(p99),
        "note": "Cloud collector footprint under a cgroup v2 cap of cap_cpus vCPU and "
                "cap_mem_mb MB; the edge collectors are capped identically. The collector "
                "binary and the 103-service workload are identical across the four "
                "production tiers, so this is the footprint of the pipeline under the cap.",
    }


def main():
    mac = parse_macros(A / "numbers.tex")
    energy = parse_energy(A / "energy.txt")
    base_bytes = parse_stats_bytes(A / "stats.txt")

    # RCAEval external benchmark (125 RE1-OB cases, service level).
    rcaeval = {}
    for row in read_csv(A / "rcaeval_comparison.csv"):
        rcaeval[row["method"]] = {
            "cases": int(fnum(row.get("cases")) or 0),
            "top1_pct": fnum(row.get("top1_pct")),
            "top3_pct": fnum(row.get("top3_pct")),
            "avg5_pct": fnum(row.get("avg5_pct")),
            "ppr_minus_this_top1": fnum(row.get("ppr_minus_this_top1")),
            "ci_lo": fnum(row.get("ci_lo")),
            "ci_hi": fnum(row.get("ci_hi")),
            "p_holm": fnum(row.get("p_holm")),
            "reject_0.05": (row.get("reject_0.05", "").strip() == "yes"),
        }

    # Multi-edge per-placement (103 services, 5 edge devices).
    me_placements = {}
    for row in read_csv(A / "summary_em.csv"):
        me_placements[row["placement"]] = {
            "n": int(fnum(row["n"]) or 0),
            "success_pct": fnum(row["success_pct"]),
            "success_ci": [fnum(row.get("success_lo")), fnum(row.get("success_hi"))],
            "edge_median_ttd_steps": fnum(row.get("edge_median_ttd")),
            "edge_p95_ttd_steps": fnum(row.get("edge_p95_ttd")),
            "edge_p99_ttd_steps": fnum(row.get("edge_p99_ttd")),
            "cloud_median_ttd_steps": fnum(row.get("cloud_median_ttd")),
            "mean_cross_kb": fnum(row.get("mean_cross_kb")),
        }

    # Per-device cells.
    devices = []
    for row in read_csv(A / "summary_em_devices.csv"):
        devices.append({
            "device": row["device"],
            "size": int(fnum(row.get("size")) or 0),
            "partition_sec": fnum(row.get("partition_sec")),
            "placement": row["placement"],
            "n": int(fnum(row["n"]) or 0),
            "edge_success_pct": fnum(row.get("edge_success_pct")),
            "median_ttd_steps": fnum(row.get("median_ttd_steps")),
            "mean_cross_kb": fnum(row.get("mean_cross_kb")),
        })

    # False-alarm sweep (fault-free telemetry, threshold h swept).
    fp_sweep = []
    for row in read_csv(P / "raw_fp.csv"):
        fp_sweep.append({
            "h": fnum(row.get("h")),
            "persist": int(fnum(row.get("persist")) or 0),
            "observation_hours": fnum(row.get("observation_hours")),
            "total_false_alarms": int(fnum(row.get("total_false_alarms")) or 0),
            "false_alarms_per_hour": fnum(row.get("false_alarms_per_hour")),
        })

    # Detector and ranker parameters (from the implementation; one place to cite).
    parameters = {
        "sampling_hz": 10, "step_ms": 100,
        "robust_iqr_to_sigma": 1.349, "flat_metric_eps": 1e-9,
        "cusum_baseline_window_steps": 15, "cusum_slack_k": 0.5,
        "cusum_threshold_h": 4.0, "cusum_persistence_steps": 3,
        "ppr_damping_alpha": 0.85, "ppr_power_iterations": 100, "ppr_self_weight": 1.0,
        "fault_per_hop_delay_steps": 2, "fault_ramp_theta_steps": 3,
        "noise_sd": 2.0, "noise_extra_per_loss": 6.0,
        "drain_bandwidth_samples_per_step": 20,
        "operating_point_h": 4.0,
    }

    STEP_MS = 500  # one production collector tick; see B2 unit reconciliation.

    results = {
        "schema": "sovloc-results/1",
        "note": "Single machine-readable source for headline numbers. Every byte or "
                "latency figure is tagged with the configuration it was measured under. "
                "The simulation samples at 10 Hz (one step = 100 ms of simulated time); "
                "the production collectors tick at 2 Hz (one tick = %d ms)." % STEP_MS,
        "sim_step_ms": 100,
        "production_tick_ms": STEP_MS,
        "runs": {
            "total": int(fnum(mac.get("numTotalRuns")) or 0),
            "e1": int(fnum(mac.get("numEoneRuns")) or 0),
        },
        "survival": {
            "central_sf_edge_slope_steps_per_step": fnum(mac.get("sfEdgeSlope")),
            "edge_local_edge_slope_steps_per_step": fnum(mac.get("elEdgeSlope")),
            "tost_diff": fnum(mac.get("tostDiff")),
            "tost_p": fnum(mac.get("tostP")),
            "logrank_chi2": fnum(mac.get("logrankChi")),
            "logrank_p": fnum(mac.get("logrankP")),
            "longest_partition_sec": fnum(mac.get("partLongestSec")),
        },
        "ranking_internal_topologies": {
            "ppr_minus_argmax_pct": fnum(mac.get("pprMinusArgmax")),
            "ppr_minus_argmax_p": fnum(mac.get("pprArgmaxP")),
            "ppr_minus_randomwalk_pct": fnum(mac.get("pprMinusRandomWalk")),
            "ppr_minus_randomwalk_p": fnum(mac.get("pprRandomWalkP")),
        },
        "rcaeval_external": {
            "dataset": "RCAEval RE1-OB, 125 cases, service level",
            "methods": rcaeval,
        },
        "bytes": {
            "base_7svc_10hz": {
                **(base_bytes or {}),
                "ratio_sf_over_edge": fnum(mac.get("bytesRatioEdge")),
                "ratio_sf_over_hybrid": fnum(mac.get("bytesRatioHybrid")),
                "config": "7 services, 10 Hz sampling, single edge",
            },
            "multiedge_103svc": {
                "central_naive_kb": me_placements.get("central-naive", {}).get("mean_cross_kb"),
                "central_sf_kb": me_placements.get("central-sf", {}).get("mean_cross_kb"),
                "edge_local_kb": me_placements.get("edge-local", {}).get("mean_cross_kb"),
                "hybrid_kb": me_placements.get("hybrid", {}).get("mean_cross_kb"),
                "ratio_sf_over_edge": fnum(mac.get("emBytesRatioEdge")),
                "ratio_sf_over_hybrid": fnum(mac.get("emBytesRatioHybrid")),
                "config": "103 services, 5 edge devices",
            },
        },
        "multiedge": {
            "services": int(fnum(mac.get("emServices")) or 0),
            "edge_devices": int(fnum(mac.get("emEdges")) or 0),
            "central_naive_edge_slope_steps_per_sec": fnum(mac.get("emCentralSlope")),
            "edge_local_edge_slope_steps_per_sec": fnum(mac.get("emEdgeSlope")),
            "placements": me_placements,
            "devices": devices,
        },
        "energy_rapl_x86": energy,
        "parameters": parameters,
        "false_alarm_sweep": {
            "fault_free_hours": fp_sweep[0]["observation_hours"] if fp_sweep else None,
            "operating_point_h": 4.0,
            "rows": fp_sweep,
        },
        "constrained_footprint": summarize_capped(P / "production" / "single-node-mesh-capped.csv"),
    }

    out = P / "results.json"
    out.write_text(json.dumps(results, indent=2) + "\n")

    # Internal consistency guard: catch a byte ratio that disagrees with its
    # own numerator/denominator, and a recomputed ratio that drifts from the macro.
    warnings = []
    bb = results["bytes"]["base_7svc_10hz"]
    if bb.get("edge_local_bytes"):
        recomputed = bb["central_sf_bytes"] / bb["edge_local_bytes"]
        if bb.get("ratio_sf_over_edge") and abs(recomputed - bb["ratio_sf_over_edge"]) > 0.15:
            warnings.append(
                f"base byte ratio {bb['ratio_sf_over_edge']} != recomputed {recomputed:.2f}")
    print(f"results.json written to {out} ({len(json.dumps(results))} bytes)")
    print(f"  runs.total={results['runs']['total']}  "
          f"rcaeval methods={list(rcaeval)}  "
          f"multiedge placements={list(me_placements)}")
    if warnings:
        print("CONSISTENCY WARNINGS:")
        for w in warnings:
            print("  !", w)
        return 1
    print("  internal byte-ratio consistency: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
