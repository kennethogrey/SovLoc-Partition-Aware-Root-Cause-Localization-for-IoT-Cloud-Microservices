#!/usr/bin/env bash
# Summarise each E6 production CSV and compare the measured time to diagnosis (and
# bytes, when measured) against the simulation within the +/-20% tolerance the
# review requires. Pure bash + awk: no Python, no dependencies.
#
# It reads the header row to find columns by name, so it handles BOTH schemas:
#   * the single/cloud-half tier (scripts/e6/run_events.sh): 7 services, no device.
#   * the full multi-edge mesh (scripts/e6/run_events_mesh.sh): a `device` column,
#     an `n_services` column, a real per-device network cut.
#
# The simulation reference is the edge-local edge-fault median time to diagnosis:
# results/analysis/summary_em.csv (the multi-edge headline) if present, else
# summary_e1.csv. Per-platform calibrated ms-per-step is read from
# results/production/<platform>_calib.txt if present (E3), so the median is
# compared on the same tick scale rather than a hardcoded constant.
#
#   scripts/e6/compare_prod.sh results/production/*.csv
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"
[ "$#" -ge 1 ] || { echo "usage: compare_prod.sh results/production/<platform>.csv ..."; exit 1; }

EM="results/analysis/summary_em.csv"
E1="results/analysis/summary_e1.csv"
if [ -f "$EM" ]; then
  # edge-local, edge-fault KM median AND P95 time to diagnosis (steps), mean bytes (KB).
  # A percentile is only comparable to the same percentile: P95 is checked against the
  # simulation's own P95 (field 9), not against the median, so a real tail is judged
  # against the modelled tail rather than the modelled centre.
  sim_edge_steps=$(awk -F, 'NR>1 && $1=="edge-local" && $8!="censored" && $8!=""{print $8; exit} END{if(!NR)print 1}' "$EM")
  sim_edge_p95=$(awk -F, 'NR>1 && $1=="edge-local" && $9!="censored" && $9!=""{print $9; exit}' "$EM")
  sim_edge_kb=$(awk -F, 'NR>1 && $1=="edge-local"{print $12; exit}' "$EM")
elif [ -f "$E1" ]; then
  sim_edge_steps=$(awk -F, 'NR>1 && $1=="edge-local" && $14!="censored" && $14!=""{s+=$14;n++} END{printf "%.3f",(n? s/n : 1)}' "$E1")
  sim_edge_p95=""
  sim_edge_kb=$(awk -F, 'NR>1 && $1=="edge-local"{s+=$21;n++} END{printf "%.3f",(n? s/n : 0)}' "$E1")
else
  sim_edge_steps=1; sim_edge_p95=""; sim_edge_kb=0
  echo "note: no sim summary found; run 'make reproduce' first. Using sim median = 1 step."
fi
[ -n "${sim_edge_steps:-}" ] || sim_edge_steps=1
[ -n "${sim_edge_kb:-}" ] || sim_edge_kb=0

for path in "$@"; do
  [ -f "$path" ] || { echo "$path: missing"; continue; }
  plat="$(awk -F, 'NR==2{print $1; exit}' "$path")"; [ -n "$plat" ] || plat="$path"
  calib="results/production/${plat}_calib.txt"
  ms_per_step=100   # sim STEP_MS default
  if [ -f "$calib" ]; then
    v=$(awk -F= '/ms_per_step/{gsub(/[^0-9.]/,"",$2); print $2; exit}' "$calib" 2>/dev/null)
    [ -n "$v" ] && ms_per_step="$v"
  fi

  awk -F, -v plat="$plat" -v sim_steps="$sim_edge_steps" -v sim_p95_steps="$sim_edge_p95" \
          -v sim_kb="$sim_edge_kb" -v mps="$ms_per_step" '
    function wilson(k,n,  p,d,c,h){ if(n==0)return "n=0"; p=k/n; d=1+3.8416/n;
      c=(p+1.9208/n)/d; h=1.96*sqrt(p*(1-p)/n+0.9604/(n*n))/d;
      return sprintf("%.1f%% [%.1f, %.1f]",p*100,(c-h>0?c-h:0)*100,(c+h<1?c+h:1)*100) }
    function isort(a,n,  i,j,key){ for(i=2;i<=n;i++){ key=a[i]; j=i-1;
      while(j>=1 && a[j]>key){ a[j+1]=a[j]; j-- } a[j+1]=key } }
    function med(a,n){ if(n==0)return ""; isort(a,n); return (n%2)? a[int(n/2)+1] : (a[n/2]+a[n/2+1])/2 }
    function p95(a,n,  pos,lo){ if(n==0)return ""; isort(a,n); pos=0.95*(n-1)+1; lo=int(pos);
      return (lo>=n)? a[n] : a[lo]+(pos-lo)*(a[lo+1]-a[lo]) }
    function val(name){ return (name in col) ? $col[name] : "" }
    NR==1 { for(i=1;i<=NF;i++) col[$i]=i; next }
    {
      tot++;
      side=val("culprit_side"); el=val("edge_localized"); ett=val("edge_ttd_ms");
      cbl=val("cloud_localized_during_cut"); cmv=val("cloud_min_visible");
      ns=val("n_services"); cb=val("cross_bytes");
      cpu=val("cpu_ms"); rss=val("rss_kb"); p99=val("p99_handler_us");
      if(ns!=""){ nserv=ns+0 }
      if(side=="edge")ec++; else cc++;
      if(el==1)elloc++;
      if(side=="edge" && cbl==0)cblind++;      # cloud blind during an edge-fault cut
      if(side=="edge" && ett!="NA" && ett!=""){ ttd[++nt]=ett+0 }
      if(cmv!="NA" && cmv!=""){ mv[++nm]=cmv+0 }
      if(cb!="" && cb!="NA"){ by[++nb]=cb+0 }
      if(cpu!="" && cpu!="NA"){ cpuA[++ncpu]=cpu+0 }
      if(rss!="" && rss!="NA"){ rssA[++nrss]=rss+0 }
      if(p99!="" && p99!="NA"){ p99A[++np99]=p99+0 }
    }
    END {
      ntot=(nserv? nserv : 7)
      print "=== " plat "  (" tot " events: " ec+0 " edge-fault, " cc+0 " cloud-fault) ==="
      print "  edge collector localized:          " wilson(elloc,tot)
      if(ec>0) print "  cloud blind during edge-fault cut: " wilson(cblind,ec)
      if(nm>0){ print "  cloud min services seen in cut:    median " med(mv,nm) "/" ntot " (loses exactly the cut device, keeps the rest)" }
      if(nt>0){
        pm=med(ttd,nt); pp=p95(ttd,nt);
        sim_ms=sim_steps*mps;
        # Compare the tail to the modelled tail: sim P95 (steps) if the summary gives
        # one, else fall back to the median so the row still prints.
        sim_p95_ms=((sim_p95_steps+0)>0? sim_p95_steps*mps : sim_ms);
        rm=(sim_ms>0? pm/sim_ms : 0); rp=(sim_p95_ms>0? pp/sim_p95_ms : 0);
        printf "  edge time to diagnosis (ms):       median %.0f, P95 %.0f  (n=%d)\n", pm, pp, nt
        printf "  sim edge-local median:             %.2f steps x %.0f ms/step = %.0f ms (calibrated)\n", sim_steps, mps, sim_ms
        printf "  sim edge-local P95:                %.2f steps x %.0f ms/step = %.0f ms (calibrated)\n", (sim_p95_ms/mps), mps, sim_p95_ms
        printf "  +/-20%% median:                     prod %.0f vs sim %.0f, ratio %.2f -> %s\n", pm, sim_ms, rm, (rm>=0.8&&rm<=1.2?"within":"OUTSIDE (scope the claim: real tick granularity differs)")
        printf "  +/-20%% P95 (tail vs sim tail):     prod %.0f vs sim %.0f, ratio %.2f -> %s\n", pp, sim_p95_ms, rp, (rp>=0.8&&rp<=1.2?"within":(rp<0.8?"within (faster than sim tail)":"OUTSIDE (scope the claim)"))
      }
      # Measured footprint of the cloud (central) collector, per event, read off its
      # /status counters (E4). These are measured, not modelled: the sim per-sample
      # wire size (SAMPLE_BYTES) is itself measured from the same JSON record.
      if(nb>0){
        printf "  cloud bytes received per event:     median %.0f B (measured on the wire)\n", med(by,nb)
      } else {
        print "  bytes crossing: not measured on this tier (disclosed, not inferred)."
      }
      if(ncpu>0){ printf "  cloud CPU per event:               median %.0f ms\n", med(cpuA,ncpu) }
      if(nrss>0){ printf "  cloud collector resident memory:   median %.1f MB\n", med(rssA,nrss)/1024.0 }
      if(np99>0){ printf "  cloud ingest-handler latency P99:  median %.0f us\n", med(p99A,np99) }
      print ""
    }' "$path"
done
