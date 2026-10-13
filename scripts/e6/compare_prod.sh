#!/usr/bin/env bash
# Summarise each E6 production CSV and compare the measured time to diagnosis and
# bytes against the simulation within the +/-20% tolerance the review requires.
#
# The simulation reference is read from results/analysis/summary_e1.csv (run
# `make reproduce` first). The per-platform calibrated milliseconds-per-step is
# read from results/production/<platform>_calib.txt if present (E3), so median
# time to diagnosis is compared on the same tick scale rather than a hardcoded
# constant. Pure bash + awk: no Python, no dependencies.
#
#   scripts/e6/compare_prod.sh results/production/*.csv
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"
[ "$#" -ge 1 ] || { echo "usage: compare_prod.sh results/production/<platform>.csv ..."; exit 1; }

SIM="results/analysis/summary_e1.csv"
if [ -f "$SIM" ]; then
  # edge-local edge-side fault: median TtD in steps (flat ~1 across partitions),
  # and modelled bytes crossing per run (KB).
  sim_edge_steps=$(awk -F, 'NR>1 && $1=="edge-local" && $14!="censored" && $14!=""{s+=$14;n++} END{printf "%.3f",(n? s/n : 1)}' "$SIM")
  sim_edge_kb=$(awk -F, 'NR>1 && $1=="edge-local"{s+=$21;n++} END{printf "%.3f",(n? s/n : 0)}' "$SIM")
else
  sim_edge_steps=1; sim_edge_kb=0
  echo "note: $SIM not found; run 'make reproduce' first. Using sim median = 1 step, bytes unknown."
fi

for path in "$@"; do
  [ -f "$path" ] || { echo "$path: missing"; continue; }
  plat="$(awk -F, 'NR==2{print $1; exit}' "$path")"; [ -n "$plat" ] || plat="$path"
  calib="results/production/${plat}_calib.txt"
  ms_per_step=100   # sim STEP_MS default
  if [ -f "$calib" ]; then
    v=$(awk -F= '/ms_per_step/{gsub(/[^0-9.]/,"",$2); print $2; exit}' "$calib" 2>/dev/null)
    [ -n "$v" ] && ms_per_step="$v"
  fi

  awk -F, -v plat="$plat" -v sim_steps="$sim_edge_steps" -v sim_kb="$sim_edge_kb" \
          -v mps="$ms_per_step" '
    function wilson(k,n,  p,d,c,h){ if(n==0)return "n=0"; p=k/n; d=1+3.8416/n;
      c=(p+1.9208/n)/d; h=1.96*sqrt(p*(1-p)/n+0.9604/(n*n))/d;
      return sprintf("%.1f%% [%.1f, %.1f]",p*100,(c-h>0?c-h:0)*100,(c+h<1?c+h:1)*100) }
    # portable insertion sort (mawk has no asort); sorts a[1..n] ascending in place
    function isort(a,n,  i,j,key){ for(i=2;i<=n;i++){ key=a[i]; j=i-1;
      while(j>=1 && a[j]>key){ a[j+1]=a[j]; j-- } a[j+1]=key } }
    function med(a,n){ if(n==0)return ""; isort(a,n); return (n%2)? a[int(n/2)+1] : (a[n/2]+a[n/2+1])/2 }
    function p95(a,n,  pos,lo){ if(n==0)return ""; isort(a,n); pos=0.95*(n-1)+1; lo=int(pos);
      return (lo>=n)? a[n] : a[lo]+(pos-lo)*(a[lo+1]-a[lo]) }
    NR>1 {
      tot++; if($4=="edge")ec++; else cc++;
      if($6==1)el++;
      if($4=="edge" && $8==0)cblind++;      # cloud blind during an edge-fault cut
      if($7!="NA" && $7!=""){ ttd[++nt]=$7+0 }
      if($10!="NA" && $10!=""){ by[++nb]=$10+0 }
      if($9!="NA" && $9!=""){ mv[++nm]=$9+0 }
    }
    END {
      print "=== " plat "  (" tot " events: " ec+0 " edge-fault, " cc+0 " cloud-fault) ==="
      print "  edge collector localized:          " wilson(el,tot)
      if(ec>0) print "  cloud blind during edge-fault cut: " wilson(cblind,ec)
      if(nm>0){ print "  cloud min services seen in cut:    median " med(mv,nm) "/7 (3/7 = kept its 3 local cloud services)" }
      if(nt>0){
        pm=med(ttd,nt); pp=p95(ttd,nt);
        sim_ms=sim_steps*mps;
        rm=(sim_ms>0? pm/sim_ms : 0); rp=(sim_ms>0? pp/sim_ms : 0);
        printf "  edge time to diagnosis (ms):       median %.0f, P95 %.0f  (n=%d)\n", pm, pp, nt
        printf "  sim edge-local median:             %.2f steps x %.0f ms/step = %.0f ms (calibrated)\n", sim_steps, mps, sim_ms
        printf "  +/-20%% median:                     prod %.0f vs sim %.0f, ratio %.2f -> %s\n", pm, sim_ms, rm, (rm>=0.8&&rm<=1.2?"within":"OUTSIDE")
        printf "  +/-20%% P95:                        prod %.0f vs sim %.0f, ratio %.2f -> %s\n", pp, sim_ms, rp, (rp>=0.8&&rp<=1.2?"within":"OUTSIDE")
      }
      if(nb>0){
        pb=med(by,nb); skb=sim_kb*1024;
        rb=(skb>0? pb/skb : 0);
        printf "  bytes crossing per event (median): prod %.0f B vs sim %.0f B, ratio %.2f -> %s\n", pb, skb, rb, (rb>=0.8&&rb<=1.2?"within":"OUTSIDE")
      } else {
        print "  bytes crossing: not measured on this tier (MEASURE_BYTES=0); disclosed, not inferred."
      }
      print ""
    }' "$path"
done
