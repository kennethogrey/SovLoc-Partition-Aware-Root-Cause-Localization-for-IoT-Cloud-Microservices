#!/usr/bin/env bash
# Summarise the E6 production CSV(s) and compare against the simulation within the
# plus or minus 20 percent tolerance. Pure Python, no dependencies.
#
#   scripts/e6/compare_prod.sh results/production/*.csv
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"
[ "$#" -ge 1 ] || { echo "usage: compare_prod.sh results/production/<platform>.csv ..."; exit 1; }

python3 - "$@" <<'PY'
import csv, sys, math

def wilson(k, n, z=1.96):
    if n == 0: return (float('nan'),)*3
    p = k/n; d = 1+z*z/n
    c = (p+z*z/(2*n))/d
    h = z*math.sqrt(p*(1-p)/n + z*z/(4*n*n))/d
    return p, max(0.0,c-h), min(1.0,c+h)

def median(xs):
    xs = sorted(xs)
    if not xs: return float('nan')
    n = len(xs); m = n//2
    return xs[m] if n%2 else (xs[m-1]+xs[m])/2

def pct(xs, q):
    xs = sorted(xs)
    if not xs: return float('nan')
    pos = q*(len(xs)-1); lo = int(pos); hi = min(lo+1, len(xs)-1)
    return xs[lo] + (pos-lo)*(xs[hi]-xs[lo])

for path in sys.argv[1:]:
    rows = list(csv.DictReader(open(path)))
    if not rows:
        print(f"{path}: empty"); continue
    plat = rows[0].get('platform', path)
    n = len(rows)
    edge = [r for r in rows if r.get('culprit_side')=='edge']
    cloud = [r for r in rows if r.get('culprit_side')=='cloud']
    el = sum(1 for r in rows if str(r.get('edge_localized','0')) in ('1','True','true'))
    # cloud collector blind (did NOT localize) during an edge-fault cut
    cblind = sum(1 for r in edge if str(r.get('cloud_localized_during_cut','0')) in ('0','False','false'))
    ttds = [float(r['edge_ttd_ms']) for r in rows
            if r.get('edge_ttd_ms') not in (None,'','NA')]
    p,lo,hi = wilson(el, n)
    print(f"=== {plat}  ({n} events: {len(edge)} edge-fault, {len(cloud)} cloud-fault) ===")
    print(f"  edge collector localized:        {el}/{n}  Wilson95 [{lo*100:.1f}, {hi*100:.1f}] %")
    if edge:
        pb,lb,hb = wilson(cblind, len(edge))
        print(f"  cloud blind during edge-fault cut: {cblind}/{len(edge)}  Wilson95 [{lb*100:.1f}, {hb*100:.1f}] %")
    if ttds:
        print(f"  edge time to diagnosis (ms): median {median(ttds):.0f}, P95 {pct(ttds,0.95):.0f}")
    # +/-20% check against the simulation edge-local median (about 1 step = 100 ms).
    SIM_EDGE_MEDIAN_MS = 100.0
    if ttds:
        ratio = median(ttds)/SIM_EDGE_MEDIAN_MS
        within = 0.8 <= ratio <= 1.2
        print(f"  sim-vs-prod edge-local median: prod {median(ttds):.0f} ms vs sim ~{SIM_EDGE_MEDIAN_MS:.0f} ms, "
              f"ratio {ratio:.2f} -> {'within' if within else 'OUTSIDE'} +/-20%")
    print()
PY
