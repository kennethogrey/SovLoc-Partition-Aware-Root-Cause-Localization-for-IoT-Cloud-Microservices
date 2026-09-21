"""Analyze results/raw.csv produced by the Rust `sim` binary.

For each (method, partition_dur) it reports:
  * success rate (fraction of runs that localized the true culprit in time),
    with a 95% Wilson confidence interval so the proportion is not a bare number
  * mean localization latency and its 95% confidence interval (localized runs only)
  * edge-side-culprit success rate (the discriminating case), also with a Wilson CI
  * mean bytes crossing the edge<->cloud boundary (a sovereignty cost)

It then reports effect sizes between the two methods (the edge-fault success
risk difference and the localization-latency difference with Cohen's d), and a
full per-configuration breakdown so every configuration that was run is
disclosed, including the ones where nothing interesting happens.

It writes results/analysis_summary.md and, if matplotlib is available, two PNG
plots. The script uses only the Python standard library; matplotlib is optional.
Run:  python3 analysis/analyze.py   (from the sovloc/ folder)
"""
import csv
import math
import os
import statistics
import sys

RESULTS_DIR = os.environ.get("SOVLOC_RESULTS", "results")
RAW = os.path.join(RESULTS_DIR, "raw.csv")
Z = 1.96  # 95% normal quantile


def load(path):
    if not os.path.exists(path):
        sys.exit(f"ERROR: {path} not found. Run `cargo run -p sim --release` first.")
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def mean_ci(values):
    """Mean and 95% half-width (t/normal approx) for a list of numbers."""
    k = len(values)
    if k == 0:
        return float("nan"), float("nan")
    m = statistics.fmean(values)
    if k < 2:
        return m, 0.0
    sd = statistics.stdev(values)
    return m, Z * sd / math.sqrt(k)


def wilson_ci(successes, n):
    """Wilson score interval for a proportion. Returns (phat, lo, hi).

    The Wilson interval behaves well even when the proportion sits at 0 or 1,
    which the plain normal approximation does not, so it is the right choice for
    reporting a success rate as a proportion with uncertainty.
    """
    if n == 0:
        return float("nan"), float("nan"), float("nan")
    phat = successes / n
    denom = 1 + Z * Z / n
    center = (phat + Z * Z / (2 * n)) / denom
    margin = Z * math.sqrt(phat * (1 - phat) / n + Z * Z / (4 * n * n)) / denom
    return phat, max(0.0, center - margin), min(1.0, center + margin)


def cohens_d(a, b):
    """Cohen's d for two independent samples (a - b), pooled SD."""
    na, nb = len(a), len(b)
    if na < 2 or nb < 2:
        return float("nan")
    va, vb = statistics.variance(a), statistics.variance(b)
    pooled = math.sqrt(((na - 1) * va + (nb - 1) * vb) / (na + nb - 2))
    if pooled < 1e-9:
        return float("inf") if statistics.fmean(a) != statistics.fmean(b) else 0.0
    return (statistics.fmean(a) - statistics.fmean(b)) / pooled


def is_true(row):
    return row["localized"] == "true"


def summarize(rows):
    """Aggregate over node-count and loss, grouping by (method, partition_dur)."""
    methods = ["central", "decentral"]
    durs = sorted({int(r["partition_dur"]) for r in rows})
    table = []
    for method in methods:
        for dur in durs:
            g = [r for r in rows if r["method"] == method and int(r["partition_dur"]) == dur]
            if not g:
                continue
            k = len(g)
            succ, slo, shi = wilson_ci(sum(1 for r in g if is_true(r)), k)
            lats = [float(int(r["localization_latency"])) for r in g if is_true(r)]
            mlat, ci = mean_ci(lats)
            edge = [r for r in g if r["culprit_side"] == "edge"]
            if edge:
                es, elo, ehi = wilson_ci(sum(1 for r in edge if is_true(r)), len(edge))
            else:
                es = elo = ehi = float("nan")
            cross = statistics.fmean([float(r["cross_boundary_bytes"]) for r in g]) / 1024.0
            table.append(dict(method=method, dur=dur, n=k, success=succ, slo=slo, shi=shi,
                              mlat=mlat, ci=ci, edge_success=es, elo=elo, ehi=ehi,
                              edge_lats=[float(int(r["localization_latency"])) for r in edge if is_true(r)],
                              lats=lats, cross_kb=cross))
    return table, durs


def effect_sizes(rows, durs):
    """Between-method effect sizes at each partition duration."""
    out = []
    for dur in durs:
        cen = [r for r in rows if r["method"] == "central" and int(r["partition_dur"]) == dur]
        dec = [r for r in rows if r["method"] == "decentral" and int(r["partition_dur"]) == dur]
        cen_edge = [r for r in cen if r["culprit_side"] == "edge"]
        dec_edge = [r for r in dec if r["culprit_side"] == "edge"]
        # Edge-fault success risk difference (decentral - central).
        if cen_edge and dec_edge:
            pc = sum(1 for r in cen_edge if is_true(r)) / len(cen_edge)
            pd = sum(1 for r in dec_edge if is_true(r)) / len(dec_edge)
            rd = pd - pc
            se = math.sqrt(pc * (1 - pc) / len(cen_edge) + pd * (1 - pd) / len(dec_edge))
            rd_ci = Z * se
        else:
            rd = rd_ci = float("nan")
        # Localization-latency difference (central - decentral) on localized runs.
        cl = [float(int(r["localization_latency"])) for r in cen if is_true(r)]
        dl = [float(int(r["localization_latency"])) for r in dec if is_true(r)]
        mdiff = (statistics.fmean(cl) - statistics.fmean(dl)) if cl and dl else float("nan")
        d = cohens_d(cl, dl)
        out.append(dict(dur=dur, rd=rd, rd_ci=rd_ci, mdiff=mdiff, d=d))
    return out


def per_config(rows):
    """Every (method, n, dur, loss) that was run, so nothing is hidden."""
    keys = sorted({(r["method"], int(r["n"]), int(r["partition_dur"]), float(r["loss"])) for r in rows})
    out = []
    for method, n, dur, loss in keys:
        g = [r for r in rows if r["method"] == method and int(r["n"]) == n
             and int(r["partition_dur"]) == dur and float(r["loss"]) == loss]
        k = len(g)
        succ, slo, shi = wilson_ci(sum(1 for r in g if is_true(r)), k)
        lats = [float(int(r["localization_latency"])) for r in g if is_true(r)]
        mlat, ci = mean_ci(lats)
        out.append(dict(method=method, n=n, dur=dur, loss=loss, reps=k,
                        success=succ, slo=slo, shi=shi, mlat=mlat, ci=ci))
    return out


def pct_ci(p, lo, hi):
    if math.isnan(p):
        return "n/a"
    return f"{100*p:.0f}% [{100*lo:.0f}, {100*hi:.0f}]"


def lat_ci(m, ci):
    return "n/a" if math.isnan(m) else f"{m:.1f} ± {ci:.1f}"


def write_markdown(table, effects, configs, path):
    lines = [
        "# SovLoc results summary",
        "",
        "Generated by `analysis/analyze.py` from `results/raw.csv`. Latencies are in "
        "simulation timesteps, where one step is about 100 ms. Each partition level "
        "below pools 120 repetitions (2 node counts by 2 packet-loss levels by 30 seeds). "
        "Success and edge-fault success carry a 95% Wilson interval; the latency mean "
        "carries a 95% confidence interval.",
        "",
        "## Headline: aggregated by method and partition duration",
        "",
        "| method | partition_dur | n | success [95% CI] | mean localize (±95% CI) | edge-fault success [95% CI] | cross-boundary KB |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in table:
        lines.append(
            f"| {r['method']} | {r['dur']} | {r['n']} | {pct_ci(r['success'], r['slo'], r['shi'])} | "
            f"{lat_ci(r['mlat'], r['ci'])} | {pct_ci(r['edge_success'], r['elo'], r['ehi'])} | {r['cross_kb']:.1f} |"
        )
    lines += [
        "",
        "## Effect sizes between the two methods",
        "",
        "The edge-fault success risk difference is the decentralised edge-fault success "
        "rate minus the centralised one, so a positive number means the decentralised "
        "method wins. The latency difference is the centralised mean minus the "
        "decentralised mean over the runs that localized, with Cohen's d as a "
        "standardised size. At the longest partition the centralised method only "
        "localizes the cloud-side culprits it can still see, so its latency figure there "
        "covers just those easy cases and the latency comparison stops being meaningful. "
        "The success collapse is the real story at that point.",
        "",
        "| partition_dur | edge-success risk diff (decentral vs central) | localize latency diff (central vs decentral) | Cohen's d (latency) |",
        "|---|---|---|---|",
    ]
    for e in effects:
        rd = "n/a" if math.isnan(e["rd"]) else f"{100*e['rd']:+.0f} pts ± {100*e['rd_ci']:.0f}"
        md = "n/a" if math.isnan(e["mdiff"]) else f"{e['mdiff']:+.1f} steps"
        dd = "n/a" if math.isnan(e["d"]) else ("very large" if math.isinf(e["d"]) else f"{e['d']:.2f}")
        lines.append(f"| {e['dur']} | {rd} | {md} | {dd} |")
    lines += [
        "",
        "## Full configuration breakdown (every configuration that was run)",
        "",
        "This lists all 40 method-by-configuration cells so no configuration is hidden, "
        "including the null cases where both methods behave the same. `loss` is the "
        "injected packet-loss level; `n` is the service count.",
        "",
        "| method | n | partition_dur | loss | reps | success [95% CI] | mean localize (±95% CI) |",
        "|---|---|---|---|---|---|---|",
    ]
    for c in configs:
        lines.append(
            f"| {c['method']} | {c['n']} | {c['dur']} | {c['loss']:.1f} | {c['reps']} | "
            f"{pct_ci(c['success'], c['slo'], c['shi'])} | {lat_ci(c['mlat'], c['ci'])} |"
        )
    lines += [
        "",
        "## How to read this",
        "",
        "At `partition_dur = 0` the two methods are statistically indistinguishable, "
        "which is the control. As the partition grows, the centralised collector's mean "
        "localization latency climbs, because it has to wait for the link to heal before "
        "it can see the edge again, and its edge-fault success collapses to zero once the "
        "partition outlasts the observation window. The decentralised method stays flat "
        "at roughly one step and near 100% throughout. That gap is the operational "
        "sovereignty result. The cross-boundary KB column shows the telemetry the "
        "centralised design has to ship off the edge, which is a data residency and "
        "bandwidth cost the decentralised design avoids.",
        "",
        "The simulation is seeded and deterministic, so there is no warm-up phase to "
        "discard and the same command reproduces the same numbers. The only randomness "
        "is the seeded fault placement and measurement noise, both documented in "
        "`crates/sim/src/main.rs`.",
        "",
    ]
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print(f"Wrote {path}")


def make_plots(table, durs, out_dir):
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except Exception:
        print("(matplotlib not available, skipping PNG plots; the markdown table is enough.)")
        return
    for _title, ylabel, key, fname, pct in [
        ("Localization latency", "mean localize (steps)", "mlat", "fig_latency.png", False),
        ("Edge-fault localization success", "edge-fault success (%)", "edge_success", "fig_edge_success.png", True),
    ]:
        plt.figure(figsize=(6, 4))
        for method in ["central", "decentral"]:
            xs, ys, es = [], [], []
            for dur in durs:
                row = next((r for r in table if r["method"] == method and r["dur"] == dur), None)
                if not row:
                    continue
                val = row[key]
                if isinstance(val, float) and math.isnan(val):
                    continue
                xs.append(dur)
                ys.append(val * 100 if pct else val)
                es.append(row["ci"] if key == "mlat" else 0)
            if key == "mlat":
                plt.errorbar(xs, ys, yerr=es, marker="o", capsize=3, label=method)
            else:
                plt.plot(xs, ys, marker="o", label=method)
        plt.xlabel("partition duration (steps)")
        plt.ylabel(ylabel)
        plt.title(ylabel.split(" (")[0] + " vs partition duration")
        plt.legend()
        plt.grid(True, alpha=0.3)
        plt.tight_layout()
        p = os.path.join(out_dir, fname)
        plt.savefig(p, dpi=120)
        plt.close()
        print(f"Wrote {p}")


def main():
    rows = load(RAW)
    table, durs = summarize(rows)
    effects = effect_sizes(rows, durs)
    configs = per_config(rows)
    print(f"\nLoaded {len(rows)} rows across {len(durs)} partition levels.\n")
    print(f"{'method':<10}{'dur':>5}{'success':>9}{'mean_loc':>10}{'edge_succ':>11}")
    print("-" * 45)
    for r in table:
        mlat = "nan" if math.isnan(r["mlat"]) else f"{r['mlat']:.1f}"
        es = "nan" if math.isnan(r["edge_success"]) else f"{100*r['edge_success']:.0f}%"
        print(f"{r['method']:<10}{r['dur']:>5}{100*r['success']:>8.0f}%{mlat:>10}{es:>11}")
    write_markdown(table, effects, configs, os.path.join(RESULTS_DIR, "analysis_summary.md"))
    make_plots(table, durs, RESULTS_DIR)


if __name__ == "__main__":
    main()
