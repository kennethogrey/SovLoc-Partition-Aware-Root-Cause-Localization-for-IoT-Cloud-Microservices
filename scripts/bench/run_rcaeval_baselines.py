#!/usr/bin/env python3
"""Run the published comparators on the bundled RCAEval RE1-OB benchmark.

This runs the ORIGINAL authors' implementations of BARO, N-sigma and CIRCA
from the RCAEval package (not re-implementations) over the same 125 Online
Boutique fault cases that the Rust `rca_bench` scores, so the external
comparison in the manuscript uses each method's own code on identical cases
and ground truth. RCD is attempted; if the installed causal-learn has dropped
the API it needs, the method is recorded as "not run" with the reason rather
than silently skipped.

Localization is service level, the same granularity as the RE1-OB ground
truth: each method returns a ranking over `<service>_<metric>` columns, which
is reduced to a service ordering by first appearance. Accuracy follows the
RCAEval Evaluator definitions:
    AC@k   = fraction of cases whose true service is in the top-k services
    Avg@5  = mean(AC@1 .. AC@5)

Outputs (under --out-dir, default results/analysis):
    rcaeval_baselines.csv          method, cases, top1_pct, top3_pct, avg5_pct, mean_s
    rcaeval_baselines_percase.csv  method, case, true_service, hit1, hit3, svc_rank
The per-case file feeds the paired bootstrap and Holm comparison against PPR.

Usage:
    run_rcaeval_baselines.py --root target/rcaeval/RE1-OB [--out-dir results/analysis]
"""
import argparse
import contextlib
import csv
import io
import os
import sys
import time
import warnings

warnings.filterwarnings("ignore")

FAULTS = ("cpu", "delay", "disk", "loss", "mem")


def true_service(case_folder):
    """Service named by a `<service>_<fault>` case folder."""
    for f in FAULTS:
        if case_folder.endswith("_" + f):
            return case_folder[: -(len(f) + 1)]
    return case_folder


def service_of(column):
    """Service a `<service>_<metric>` column belongs to."""
    return str(column).rsplit("_", 1)[0]


def service_ranking(ranks):
    """Reduce a metric-level ranking to a service ordering by first appearance."""
    out = []
    for r in ranks:
        s = service_of(r)
        if s and s not in out:
            out.append(s)
    return out


def service_rank_of(ranks, svc):
    """1-based position of `svc` in the service ranking, or None if absent."""
    order = service_ranking(ranks)
    return order.index(svc) + 1 if svc in order else None


def list_cases(root):
    cases = []
    for case_folder in sorted(os.listdir(root)):
        cdir = os.path.join(root, case_folder)
        if not os.path.isdir(cdir):
            continue
        for run in sorted(os.listdir(cdir)):
            rdir = os.path.join(cdir, run)
            data = os.path.join(rdir, "data.csv")
            inj = os.path.join(rdir, "inject_time.txt")
            if os.path.isfile(data) and os.path.isfile(inj):
                cases.append((f"{case_folder}_{run}", true_service(case_folder), data, inj))
    return cases


def build_methods():
    """Each method: name -> callable(data_df, inject_time) -> ranks list."""
    from RCAEval.e2e import baro, nsigma, circa, rcd

    def run_baro(df, it):
        return baro(df.copy(), inject_time=it)["ranks"]

    def run_nsigma(df, it):
        return nsigma(df.copy(), inject_time=it)["ranks"]

    def run_circa(df, it):
        return circa(df.copy(), inject_time=it, dataset="online-boutique")["ranks"]

    def run_rcd(df, it):
        return rcd(df.copy(), it)["ranks"]

    return [
        ("baro", run_baro),
        ("nsigma", run_nsigma),
        ("circa", run_circa),
        ("rcd", run_rcd),
    ]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", default="target/rcaeval/RE1-OB")
    ap.add_argument("--out-dir", default="results/analysis")
    ap.add_argument("--metric-filter", default=None,
                    help="keep only `time` + columns ending _<METRIC> (e.g. latency), for a "
                         "same-input comparison with the latency-only Rust rankers")
    ap.add_argument("--tag", default="", help="suffix for output filenames, e.g. _latency")
    args = ap.parse_args()

    import pandas as pd

    cases = list_cases(args.root)
    if not cases:
        sys.exit(f"no RE1-OB cases under {args.root} (run `make bench` once to unpack)")
    methods = build_methods()
    os.makedirs(args.out_dir, exist_ok=True)

    # per-case rows and per-method rank lists (rank = None when not placed / errored)
    percase = []
    ranks = {name: [] for name, _ in methods}
    dead = {}  # method -> reason it was not run on any case

    print(f"RCAEval comparators on {len(cases)} RE1-OB cases:")
    failures = {name: 0 for name, _ in methods}  # method's own internal fallbacks
    for cid, svc, data_path, inj_path in cases:
        df_raw = pd.read_csv(data_path)
        # Clean NaN the way a careful benchmark does: forward then backward fill,
        # then zero. The time column has no gaps; a constant column a method cannot
        # use it drops itself (drop_constant). This removes the "input contains NaN"
        # failures; a genuinely singular causal case still falls back and is counted.
        df = df_raw.ffill().bfill().fillna(0.0)
        if args.metric_filter:
            suf = "_" + args.metric_filter
            df = df[["time"] + [c for c in df.columns if c.endswith(suf)]]
        it = int(open(inj_path).read().strip())
        for name, fn in methods:
            if name in dead:
                continue
            try:
                err = io.StringIO()
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                    r = fn(df, it)
                failed = "failed" in err.getvalue().lower()  # RCAEval's own fallback marker
                if failed:
                    failures[name] += 1
                sr = service_rank_of(r, svc)
                ranks[name].append(sr)
                percase.append([name, cid, svc, int(sr == 1), int(sr is not None and sr <= 3),
                                sr if sr is not None else "", int(failed)])
            except Exception as e:  # a hard error (e.g. a missing dependency) kills the method
                dead[name] = f"{type(e).__name__}: {str(e)[:120]}"
                print(f"  {name}: NOT RUN ({dead[name]})", file=sys.stderr)

    def ac(rs, k):
        placed = [r for r in rs if r is not None]
        return 100.0 * sum(1 for r in rs if r is not None and r <= k) / len(rs) if rs else 0.0

    sumrows = []
    for name, _ in methods:
        rs = ranks[name]
        if not rs:
            sumrows.append([name, 0, "", "", "", "", dead.get(name, "no cases")])
            continue
        ac1, ac3 = ac(rs, 1), ac(rs, 3)
        avg5 = sum(ac(rs, k) for k in range(1, 6)) / 5.0
        meank = sum(r for r in rs if r is not None) / max(1, sum(1 for r in rs if r is not None))
        note = f"{failures[name]} internal fallback(s)" if failures.get(name) else ""
        sumrows.append([name, len(rs), f"{ac1:.1f}", f"{ac3:.1f}", f"{avg5:.1f}", f"{meank:.2f}", note])

    spath = os.path.join(args.out_dir, f"rcaeval_baselines{args.tag}.csv")
    with open(spath, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["method", "cases", "top1_pct", "top3_pct", "avg5_pct", "mean_service_rank", "note"])
        w.writerows(sumrows)

    ppath = os.path.join(args.out_dir, f"rcaeval_baselines_percase{args.tag}.csv")
    with open(ppath, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["method", "case", "true_service", "hit1", "hit3", "svc_rank", "failed"])
        w.writerows(percase)

    print(f"\n{'method':8s} {'n':>4s} {'top1':>6s} {'top3':>6s} {'Avg@5':>6s}")
    for row in sumrows:
        print(f"{row[0]:8s} {str(row[1]):>4s} {str(row[2]):>6s} {str(row[3]):>6s} {str(row[4]):>6s}")
    for name, _ in methods:
        if failures.get(name):
            print(f"NOTE: {name} fell back to column order on {failures[name]} case(s) (its own causal step failed)")
    if dead:
        for name, why in dead.items():
            print(f"NOTE: {name} not run -> {why}")
    print(f"Wrote {spath} and {ppath}")


if __name__ == "__main__":
    main()
