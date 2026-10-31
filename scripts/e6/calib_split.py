#!/usr/bin/env python3
"""Repeated random-split calibration validation of the simulation against production.

For each production tier the edge-fault events the edge collector localized are
repeatedly split at random into a calibration half and a held-out validation half
(NSPLITS seeded splits). The simulation predicts an edge time to diagnosis of one
step at the median and two steps at the P95 (results/results.json,
multiedge.placements.edge-local). On each split we fit the one free constant,
milliseconds per step, on the calibration half only (ms_per_step = median edge TtD
there, since the simulated median is one step), then score the prediction on the
held-out half:

    ratio_median = (1 step * ms_per_step) / median(validation edge TtD)
    ratio_p95    = (2 step * ms_per_step) / P95(validation edge TtD)

We report the median ratio over the random splits and its central 95 percent spread
([2.5, 97.5] percentiles over the splits). Random splits are used rather than a
single first/last-half split because the capture order carries a warmup trend (the
earliest events run slower while caches, connections and schedulers settle), which a
position split would confound with the calibration; random splits balance any such
trend across the two halves and remove single-split luck. A ratio in [0.8, 1.2] means
the calibrated simulation predicts the held-out half within twenty percent. Where the
tier records per-event bytes, a production-side split-half stability ratio for bytes
is reported alongside, labelled as such.

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
NSPLITS = 2000
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


def split_ratios(values, predict_steps, stat_fn):
    """Median held-out ratio and [2.5, 97.5] spread over NSPLITS random half-splits.

    On each split the calibration half fixes ms_per_step (its median), and the
    prediction predict_steps * ms_per_step is scored against stat_fn of the held-out
    half. Seeded, so the output is deterministic.
    """
    rng = random.Random(SEED)
    n = len(values)
    h = n // 2
    ratios = []
    for _ in range(NSPLITS):
        s = values[:]
        rng.shuffle(s)
        cal, val = s[:h], s[h:]
        denom = stat_fn(val)
        if denom and denom == denom:
            ratios.append((predict_steps * st.median(cal)) / denom)
    return st.median(ratios), pct(ratios, 0.025), pct(ratios, 0.975)


def main():
    # Simulated edge-local prediction, in steps, from the single source.
    results = json.loads((ROOT / "results" / "results.json").read_text())
    el = results["multiedge"]["placements"]["edge-local"]
    sim_median_steps = el["edge_median_ttd_steps"]  # 1
    sim_p95_steps = el["edge_p95_ttd_steps"]         # 2

    out_rows = []
    print(f"Repeated random-split calibration ({NSPLITS} splits; sim edge median "
          f"{sim_median_steps:g} step, P95 {sim_p95_steps:g} steps); pass band {BAND}")
    print(f"{'tier':16} {'metric':11} {'ratio':>6} {'95% spread':>18}  verdict")
    for tier in TIERS:
        ttd, byts = load_edge(tier)
        if len(ttd) < 20:
            print(f"{tier:16} only {len(ttd)} edge events; skipping")
            continue
        ms_ref = st.median(ttd)  # representative calibration constant (full-sample median)
        r_med, lo_m, hi_m = split_ratios(ttd, sim_median_steps, st.median)
        r_p95, lo_p, hi_p = split_ratios(ttd, sim_p95_steps, lambda s: pct(s, 0.95))

        for metric, ratio, lo, hi in [("median_ttd", r_med, lo_m, hi_m),
                                      ("p95_ttd", r_p95, lo_p, hi_p)]:
            verdict = "in band" if BAND[0] <= ratio <= BAND[1] else "OUT of band"
            print(f"{tier:16} {metric:11} {ratio:6.2f} "
                  f"[{lo:5.2f}, {hi:5.2f}]{'':>3}  {verdict}")
            out_rows.append([tier, metric, f"{ms_ref:.1f}", f"{ratio:.3f}",
                             f"{lo:.3f}", f"{hi:.3f}",
                             "yes" if BAND[0] <= ratio <= BAND[1] else "no"])

        if len(byts) >= 20:  # production-side bytes stability (mesh tier)
            r_b, lo_b, hi_b = split_ratios(byts, 1.0, st.median)
            verdict = "in band" if BAND[0] <= r_b <= BAND[1] else "OUT of band"
            print(f"{tier:16} {'bytes_evt*':11} {r_b:6.2f} "
                  f"[{lo_b:5.2f}, {hi_b:5.2f}]{'':>3}  {verdict}  (*split-half stability)")
            out_rows.append([tier, "bytes_per_event_stability", "", f"{r_b:.3f}",
                             f"{lo_b:.3f}", f"{hi_b:.3f}",
                             "yes" if BAND[0] <= r_b <= BAND[1] else "no"])

    out = PROD / "calib_split.csv"
    with out.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["tier", "metric", "ms_per_step_median", "ratio",
                    "ci_lo", "ci_hi", "in_band_0.8_1.2"])
        w.writerows(out_rows)
    print(f"\nWrote {out}")


if __name__ == "__main__":
    main()
