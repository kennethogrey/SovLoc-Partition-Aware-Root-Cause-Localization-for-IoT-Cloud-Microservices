#!/usr/bin/env python3
"""Compare PPR against every other ranker on the RE1-OB benchmark, paired by case.

Joins the per-case output of the Rust `rca_bench` (argmax, random-walk, PPR) with
the per-case output of the published comparators (BARO, N-sigma, CIRCA, run by
run_rcaeval_baselines.py with their authors' own code) on the shared `case` key, so
every method is scored on the same cases and every PPR-vs-method contrast is paired.

Reports, over the common cases:
    top-1 (AC@1), top-3 (AC@3), Avg@5 = mean(AC@1..AC@5)  per method
    PPR minus method top-1, with a 95% bootstrap CI over cases, a two-sided
    bootstrap p-value, and a Holm-adjusted p-value over the family of contrasts.

Stdlib only, so it runs under the system Python. Deterministic (fixed seed).

Usage:
    compare_rankers.py [--dir results/analysis] [--boot 10000] [--seed 20260127]
"""
import argparse
import csv
import os
import random


def load_percase(path):
    """case -> {method: svc_rank or None} from a *_percase.csv."""
    out = {}
    if not os.path.exists(path):
        return out
    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            case = row["case"]
            sr = row.get("svc_rank", "")
            rank = int(sr) if sr not in ("", None) else None
            out.setdefault(case, {})[row["method"]] = rank
    return out


def ac_at(ranks, k):
    """AC@k = fraction of cases whose culprit is in the top k."""
    return sum(1 for r in ranks if r is not None and r <= k) / len(ranks) if ranks else 0.0


def boot_ci_p(diffs, n_boot, seed):
    """Mean of paired diffs with a percentile 95% CI and a two-sided bootstrap p."""
    if not diffs:
        return 0.0, 0.0, 0.0, 1.0
    rng = random.Random(seed)
    m = sum(diffs) / len(diffs)
    means = []
    n = len(diffs)
    for _ in range(n_boot):
        s = sum(diffs[rng.randrange(n)] for _ in range(n)) / n
        means.append(s)
    means.sort()
    lo = means[int(0.025 * n_boot)]
    hi = means[int(0.975 * n_boot) - 1]
    below = sum(1 for x in means if x <= 0.0) / n_boot
    p = 2.0 * min(below, 1.0 - below)
    return m, lo, hi, p


def holm(pvals):
    """Holm step-down adjusted p-values, returned in the original order."""
    order = sorted(range(len(pvals)), key=lambda i: pvals[i])
    adj = [0.0] * len(pvals)
    run = 0.0
    for rank, i in enumerate(order):
        val = (len(pvals) - rank) * pvals[i]
        run = max(run, val)
        adj[i] = min(1.0, run)
    return adj


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dir", default="results/analysis")
    ap.add_argument("--boot", type=int, default=10000)
    ap.add_argument("--seed", type=int, default=20260127)
    ap.add_argument("--reference", default="ppr")
    ap.add_argument("--baselines-file", default="rcaeval_baselines_percase.csv",
                    help="authors' per-case file to join against "
                         "(e.g. rcaeval_baselines_percase_latency.csv for the same-input comparison)")
    args = ap.parse_args()

    ours = load_percase(os.path.join(args.dir, "rca_bench_percase.csv"))
    theirs = load_percase(os.path.join(args.dir, args.baselines_file))
    if not ours:
        raise SystemExit("missing rca_bench_percase.csv (run `make bench`)")

    # Merge method rankings per case; keep cases present in both sources.
    cases = sorted(set(ours) & set(theirs)) if theirs else sorted(ours)
    methods = []
    for src in (ours, theirs):
        for c in cases:
            for m in src.get(c, {}):
                if m not in methods:
                    methods.append(m)
    ref = args.reference
    if ref not in methods:
        raise SystemExit(f"reference method '{ref}' not found in {methods}")

    def ranks_for(m):
        col = []
        for c in cases:
            r = ours.get(c, {}).get(m)
            if r is None and theirs:
                r = theirs.get(c, {}).get(m)
            col.append(r)
        return col

    ref_ranks = ranks_for(ref)
    rows, pvals, order = [], [], []
    for m in methods:
        rk = ranks_for(m)
        top1, top3 = 100 * ac_at(rk, 1), 100 * ac_at(rk, 3)
        avg5 = 100 * sum(ac_at(rk, k) for k in range(1, 6)) / 5.0
        if m == ref:
            rows.append([m, len(cases), f"{top1:.1f}", f"{top3:.1f}", f"{avg5:.1f}",
                         "-", "-", "-", "-", "-", "-"])
            continue
        diffs = [(1 if (a is not None and a == 1) else 0) - (1 if (b is not None and b == 1) else 0)
                 for a, b in zip(ref_ranks, rk)]
        d, lo, hi, p = boot_ci_p([float(x) for x in diffs], args.boot, args.seed + len(m))
        rows.append([m, len(cases), f"{top1:.1f}", f"{top3:.1f}", f"{avg5:.1f}",
                     f"{100*d:+.1f}", f"{100*lo:+.1f}", f"{100*hi:+.1f}", f"{p:.4f}"])
        pvals.append(p)
        order.append(len(rows) - 1)

    adj = holm(pvals)
    for j, ri in enumerate(order):
        rows[ri].append(f"{adj[j]:.4f}")
        rows[ri].append("yes" if adj[j] < 0.05 else "no")

    out = os.path.join(args.dir, "rcaeval_comparison.csv")
    hdr = ["method", "cases", "top1_pct", "top3_pct", "avg5_pct",
           f"{ref}_minus_this_top1", "ci_lo", "ci_hi", "p_raw", "p_holm", "reject_0.05"]
    with open(out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(hdr)
        w.writerows(rows)

    print(f"Paired ranker comparison on {len(cases)} common RE1-OB cases "
          f"(reference = {ref}, {args.boot} bootstrap resamples):\n")
    print(f"{'method':11s}{'top1':>7s}{'top3':>7s}{'Avg@5':>7s}   "
          f"{ref}-this top1 [95% CI]   p_raw   p_Holm  reject")
    for r in rows:
        tail = ("" if r[5] == "-"
                else f"{r[5]:>6s}% [{r[6]}, {r[7]}]   {r[8]:>6s}  {r[9]:>6s}   {r[10]}")
        print(f"{r[0]:11s}{r[2]:>7s}{r[3]:>7s}{r[4]:>7s}   {tail}")
    print(f"\nWrote {out}")


if __name__ == "__main__":
    main()
