#!/usr/bin/env python3
"""Held-out 15/15 calibration validation of the simulation against production.

For each production tier the 30 partition events (order randomized at capture) are
split into a calibration half (events 1..15) and a validation half (events 16..30).
The simulation predicts an edge time to diagnosis of one step at the median and two
steps at the P95 (results/results.json, multiedge.placements.edge-local). We fit the
one free constant, milliseconds per step, on the calibration half only
(ms_per_step = median edge TtD on the calibration half, since the simulated median
is one step), then score the prediction on the held-out validation half:

    ratio_median = (1 step * ms_per_step) / median(validation edge TtD)
    ratio_p95    = (2 step * ms_per_step) / P95(validation edge TtD)

A ratio in [0.8, 1.2] means the calibrated simulation predicts the held-out half
within twenty percent. The confidence interval is a nonparametric bootstrap over
the validation events (the calibration-fitted prediction held fixed). Where the
tier records per-event bytes, a production-side split-half stability ratio for
bytes per event is reported alongside, labelled as such.

Writes results/production/calib_split.csv and prints a summary.
"""
import csv
import json
import random
import statistics as st
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROD = ROOT / "results" / "production"
TIERS = ["single-node-mesh", "k3s", "openstack", "cranecloud"]
BOOT = 10000
SEED = 20260128
BAND = (0.8, 1.2)


def pct(xs, q):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    i = min(len(xs) - 1, max(0, int(round(q * (len(xs) - 1)))))
    return xs[i]


def load_edge(tier):
    path = PROD / f"{tier}.csv"
    rows = list(csv.DictReader(path.open()))
    ttd, byts = [], []
    for r in rows:
        if r.get("culprit_side") != "edge":
            continue
        if r.get("edge_localized") not in ("1", "1.0"):
            continue
        try:
            ttd.append(float(r["edge_ttd_ms"]))
        except (KeyError, ValueError):
            continue
        if r.get("cross_bytes"):
            try:
                byts.append(float(r["cross_bytes"]))
            except ValueError:
                pass
    return ttd, byts


def bootstrap_ratio(predict, validate_stat_fn, validate, lo=0.025, hi=0.975):
    """Bootstrap CI of predict / stat(resampled validate)."""
    rng = random.Random(SEED)
    ratios = []
    n = len(validate)
    for _ in range(BOOT):
        sample = [validate[rng.randrange(n)] for _ in range(n)]
        s = validate_stat_fn(sample)
        if s and s == s:
            ratios.append(predict / s)
    ratios.sort()
    return pct(ratios, lo), pct(ratios, hi)


def main():
    # Simulated edge-local prediction, in steps, from the single source.
    results = json.loads((ROOT / "results" / "results.json").read_text())
    el = results["multiedge"]["placements"]["edge-local"]
    sim_median_steps = el["edge_median_ttd_steps"]  # 1
    sim_p95_steps = el["edge_p95_ttd_steps"]         # 2

    out_rows = []
    print(f"Held-out 15/15 calibration validation (sim edge median "
          f"{sim_median_steps:g} step, P95 {sim_p95_steps:g} steps); pass band {BAND}")
    print(f"{'tier':16} {'metric':11} {'ratio':>6} {'95% CI':>18}  verdict")
    for tier in TIERS:
        ttd, byts = load_edge(tier)
        if len(ttd) < 20:
            print(f"{tier:16} only {len(ttd)} edge events; skipping")
            continue
        half = len(ttd) // 2                              # 30 -> 15/15, 29 -> 14/15
        cal, val = ttd[:half], ttd[half:]
        ms_per_step = st.median(cal)                      # calibrate on first half
        pred_median = sim_median_steps * ms_per_step
        pred_p95 = sim_p95_steps * ms_per_step

        r_med = pred_median / st.median(val)
        lo_m, hi_m = bootstrap_ratio(pred_median, st.median, val)
        r_p95 = pred_p95 / pct(val, 0.95)
        lo_p, hi_p = bootstrap_ratio(pred_p95, lambda s: pct(s, 0.95), val)

        for metric, ratio, lo, hi in [("median_ttd", r_med, lo_m, hi_m),
                                      ("p95_ttd", r_p95, lo_p, hi_p)]:
            verdict = "in band" if BAND[0] <= ratio <= BAND[1] else "OUT of band"
            print(f"{tier:16} {metric:11} {ratio:6.2f} "
                  f"[{lo:5.2f}, {hi:5.2f}]{'':>3}  {verdict}")
            out_rows.append([tier, metric, f"{ms_per_step:.1f}", f"{ratio:.3f}",
                             f"{lo:.3f}", f"{hi:.3f}",
                             "yes" if BAND[0] <= ratio <= BAND[1] else "no"])

        if len(byts) >= 20:  # production-side bytes stability (mesh tier)
            hb = len(byts) // 2
            bcal, bval = byts[:hb], byts[hb:]
            r_b = st.median(bcal) / st.median(bval)
            lo_b, hi_b = bootstrap_ratio(st.median(bcal), st.median, bval)
            verdict = "in band" if BAND[0] <= r_b <= BAND[1] else "OUT of band"
            print(f"{tier:16} {'bytes_evt*':11} {r_b:6.2f} "
                  f"[{lo_b:5.2f}, {hi_b:5.2f}]{'':>3}  {verdict}  (*split-half stability)")
            out_rows.append([tier, "bytes_per_event_stability", "", f"{r_b:.3f}",
                             f"{lo_b:.3f}", f"{hi_b:.3f}",
                             "yes" if BAND[0] <= r_b <= BAND[1] else "no"])

    out = PROD / "calib_split.csv"
    with out.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["tier", "metric", "ms_per_step_calib", "ratio",
                    "ci_lo", "ci_hi", "in_band_0.8_1.2"])
        w.writerows(out_rows)
    print(f"\nWrote {out}")


if __name__ == "__main__":
    main()
