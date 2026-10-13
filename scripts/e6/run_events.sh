#!/usr/bin/env bash
# E6 multi-event harness. Runs EVENTS independent partition events against a
# deployed SovLoc mesh and records one row per event (not per status check) to
# results/production/$PLATFORM.csv, in a schema the analysis can compare with the
# simulation.
#
#   PLATFORM=k3s CLOUD_URL=http://host:port EVENTS=30 scripts/e6/run_events.sh
#
# Per event: start the local edge services + edge collector streaming to both the
# local edge collector (TCP) and the deployed cloud collector (CLOUD_URL, HTTP);
# freeze a baseline; inject a random culprit; cut the edge->cloud link for a random
# duration using CUT_CMD / HEAL_CMD; poll both collectors; heal; write one row.
# Events are counted, not status checks. This reuses the proven single-event mesh
# commands; it NEEDS VERIFICATION on your infra, where only the cut mechanism
# differs (routed black-hole vs k8s NetworkPolicy vs OpenStack security group).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"

PLATFORM="${PLATFORM:?set PLATFORM (single-node|k3s|openstack|cranecloud)}"
CLOUD_URL="${CLOUD_URL:?set CLOUD_URL (the deployed cloud collector, http://...)}"
CLOUD_URL="${CLOUD_URL%/}"
EVENTS="${EVENTS:-30}"
BASELINE_WAIT="${BASELINE_WAIT:-12}"
PERIOD_MS="${PERIOD_MS:-200}"
STALE_MS="${STALE_MS:-3000}"
# CUT_CMD/HEAL_CMD default to the routed black-hole for a CLOUD_URL host; override
# for k3s (kubectl apply/delete a deny NetworkPolicy) or OpenStack (security group).
CLOUD_HOST="$(printf '%s' "$CLOUD_URL" | sed -E 's#https?://([^:/]+).*#\1#')"
CUT_CMD="${CUT_CMD:-scripts/cranecloud/partition_link.sh $CLOUD_HOST __PSEC__}"
HEAL_CMD="${HEAL_CMD:-true}"   # partition_link.sh self-heals after its window

BIN="target/release/mesh"
[ -x "$BIN" ] || { echo "Build first: cargo build --release -p mesh"; exit 1; }

OUT="results/production/${PLATFORM}.csv"
mkdir -p results/production logs
echo "platform,event,culprit,culprit_side,partition_sec,edge_localized,edge_ttd_ms,cloud_localized_during_cut,cloud_min_visible" > "$OUT"

EDGE_SVCS=(gateway ingest validate device-registry)
ALL=(gateway ingest validate device-registry enrich store alerting)
SEED="${SEED:-42}"; RANDOM=$SEED
PART_CHOICES=(10 30 60 120)
side_of(){ for e in "${EDGE_SVCS[@]}"; do [ "$e" = "$1" ] && { echo edge; return; }; done; echo cloud; }

for ev in $(seq 1 "$EVENTS"); do
  culprit="${ALL[$((RANDOM % ${#ALL[@]}))]}"; side="$(side_of "$culprit")"
  psec="${PART_CHOICES[$((RANDOM % ${#PART_CHOICES[@]}))]}"
  ts="$(date +%Y%m%d_%H%M%S)"; elog="logs/e6_${PLATFORM}_ev${ev}_${culprit}_${ts}.edge"
  ccsv="results/production/${PLATFORM}_ev${ev}_${ts}.cloud.csv"
  echo "event $ev/$EVENTS culprit=$culprit($side) partition=${psec}s" >&2

  pkill -f "$BIN service --name" 2>/dev/null || true
  pkill -f "$BIN collector --name edge" 2>/dev/null || true; sleep 0.4
  pids=()
  "$ROOT/scripts/cranecloud/poll_cloud_status.sh" "$CLOUD_URL" "$((psec+BASELINE_WAIT+20))" "$ccsv" >/dev/null 2>&1 & pids+=($!)
  "$BIN" collector --name edge --port 7001 --control-port 9201 --stale-ms "$STALE_MS" >"$elog" 2>&1 & pids+=($!)
  sleep 0.6
  i=0; for s in "${ALL[@]}"; do cp=$((9110+i)); i=$((i+1))
    "$BIN" service --name "$s" --culprit none --control-port "$cp" --period-ms "$PERIOD_MS" \
       --edge 127.0.0.1:7001 --http-cloud "$CLOUD_URL" >"logs/e6_${PLATFORM}_$s.log" 2>&1 & pids+=($!)
  done
  sleep "$BASELINE_WAIT"
  "$BIN" inject --culprit "$culprit" \
    --services 127.0.0.1:9110,127.0.0.1:9111,127.0.0.1:9112,127.0.0.1:9113,127.0.0.1:9114,127.0.0.1:9115,127.0.0.1:9116 \
    --collectors 127.0.0.1:9201 --http-collectors "$CLOUD_URL" >/dev/null 2>&1
  # cut the link for psec seconds (self-healing), then observe a little longer
  eval "${CUT_CMD/__PSEC__/$psec}" >/dev/null 2>&1 || echo "  cut command failed; apply CUT_CMD manually" >&2
  sleep 15
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done

  # edge: did it hold the culprit at rank 1 after the fault?
  edge_loc=$(grep -cE "top-1 = ${culprit}|culprit\(${culprit}\)=rank 1" "$elog" 2>/dev/null || echo 0)
  edge_ttd=$(grep -oE "LOCALIZATION[^0-9]*([0-9]+) ?ms" "$elog" 2>/dev/null | grep -oE '[0-9]+' | head -1)
  # cloud: minimum visible during the window, and whether it localized during the cut
  cloud_minvis=$(awk -F, 'NR>1 && $3!=""{if(m==""||$3+0<m)m=$3+0} END{print (m==""?"NA":m)}' "$ccsv" 2>/dev/null)
  cloud_loc=$(awk -F, 'NR>1 && $4!="" && $4!="null"{c=1} END{print c+0}' "$ccsv" 2>/dev/null)
  echo "${PLATFORM},${ev},${culprit},${side},${psec},$([ "${edge_loc:-0}" -gt 0 ] && echo 1 || echo 0),${edge_ttd:-NA},${cloud_loc:-0},${cloud_minvis:-NA}" >> "$OUT"
done
echo "Done. Per-event rows in $OUT; compare with scripts/e6/compare_prod.sh $OUT"
