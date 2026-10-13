#!/usr/bin/env bash
# E3 calibration: measure the real per-hop delay and the real baseline diagnosis
# latency on a platform, then write the milliseconds-per-step that makes the
# simulation and the production comparable on the same tick scale. compare_prod.sh
# reads results/production/<platform>_calib.txt and applies the +/-20% rule with it,
# so the sim is CALIBRATED to the measurement rather than the check being skipped
# (the review's E3 point).
#
# Method:
#   1. per-hop delay: median of PROBES HTTP round trips to the cloud collector.
#   2. baseline diagnosis latency: a short no-partition run (CALIB_EVENTS events,
#      cut length 0) measured end to end, median edge LOCALIZATION_TIME_MS.
#   3. ms_per_step = baseline median edge TtD (ms) / simulated edge-local median (steps).
#
#   PLATFORM=single-node scripts/e6/calibrate.sh
#   PLATFORM=cranecloud CLOUD_URL=http://<app>.cranecloud.io scripts/e6/calibrate.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"
PLATFORM="${PLATFORM:?set PLATFORM (single-node|k3s|openstack|cranecloud)}"
PROBES="${PROBES:-20}"
CALIB_EVENTS="${CALIB_EVENTS:-5}"
mkdir -p results/production

if [ "$PLATFORM" = "single-node" ]; then
  CLOUD_URL="${CLOUD_URL:-http://127.0.0.1:8080}"; CMODE=local
else
  CLOUD_URL="${CLOUD_URL:?set CLOUD_URL (the deployed cloud collector, http://host:port)}"; CMODE=poll
fi
CLOUD_URL="${CLOUD_URL%/}"

# ---- 1. per-hop round-trip delay to the cloud collector ---------------------
echo "Probing $CLOUD_URL/healthz x$PROBES for per-hop delay..." >&2
rtts=""
for _ in $(seq 1 "$PROBES"); do
  t=$(curl -s -o /dev/null -m 5 -w '%{time_total}' "$CLOUD_URL/healthz" 2>/dev/null || echo "")
  [ -n "$t" ] && rtts="$rtts $(awk -v x="$t" 'BEGIN{printf "%.1f", x*1000}')"
  sleep 0.2
done
rtt_med=$(printf '%s\n' $rtts | sort -n | awk '{a[NR]=$1} END{if(NR==0){print "NA";exit} print (NR%2)?a[int(NR/2)+1]:(a[NR/2]+a[NR/2+1])/2}')

# ---- 2. baseline diagnosis latency (no-partition short run) -----------------
echo "Measuring baseline diagnosis latency ($CALIB_EVENTS no-partition events)..." >&2
PLATFORM="${PLATFORM}__calib" CLOUD_MODE="$CMODE" CLOUD_URL="$CLOUD_URL" EVENTS="$CALIB_EVENTS" CUT_SECS="0" \
  CUT_CMD='true' HEAL_CMD='true' OBSERVE_AFTER="${OBSERVE_AFTER:-6}" \
  ./scripts/e6/run_events.sh >/dev/null 2>&1 || true
CAL_CSV="results/production/${PLATFORM}__calib.csv"
base_med=$(awk -F, 'NR>1 && $7!="NA" && $7!=""{a[++n]=$7+0}
  END{ if(n==0){print "NA";exit}
       for(i=2;i<=n;i++){k=a[i];j=i-1;while(j>=1&&a[j]>k){a[j+1]=a[j];j--}a[j+1]=k}
       print (n%2)?a[int(n/2)+1]:(a[n/2]+a[n/2+1])/2 }' "$CAL_CSV" 2>/dev/null)

# ---- 3. sim edge-local median steps, then ms_per_step -----------------------
SIM="results/analysis/summary_e1.csv"
sim_steps=$(awk -F, 'NR>1 && $1=="edge-local" && $14!="censored" && $14!=""{s+=$14;n++} END{printf "%.3f",(n? s/n : 1)}' "$SIM" 2>/dev/null)
[ -n "$sim_steps" ] || sim_steps=1
if [ "$base_med" != "NA" ] && [ -n "$base_med" ]; then
  ms_per_step=$(awk -v b="$base_med" -v s="$sim_steps" 'BEGIN{printf "%.1f",(s>0? b/s : b)}')
else
  ms_per_step=100.0
fi

OUT="results/production/${PLATFORM}_calib.txt"
{
  echo "# E3 calibration for $PLATFORM"
  echo "recorded=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "cloud_url=$CLOUD_URL"
  echo "per_hop_rtt_ms_median=$rtt_med   # median of $PROBES HTTP round trips"
  echo "baseline_edge_ttd_ms_median=$base_med   # $CALIB_EVENTS no-partition events"
  echo "sim_edge_local_median_steps=$sim_steps"
  echo "ms_per_step=$ms_per_step   # used by compare_prod.sh to put sim and prod on one scale"
} > "$OUT"
echo "Wrote $OUT:"; cat "$OUT"
