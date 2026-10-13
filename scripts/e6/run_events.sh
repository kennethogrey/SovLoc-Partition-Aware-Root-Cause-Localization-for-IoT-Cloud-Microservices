#!/usr/bin/env bash
# E6 production harness: run EVENTS independent partition events against a real
# SovLoc mesh and record ONE ROW PER EVENT (not per status check) to
# results/production/$PLATFORM.csv, in the same schema the simulation uses so
# scripts/e6/compare_prod.sh can apply the +/-20% sim-vs-production rule.
#
# The seven services are split edge/cloud exactly as in the simulation:
#   edge  : gateway, ingest, validate, device-registry   (+ the edge collector)
#   cloud : enrich, store, alerting                       (+ the cloud collector)
# so that during a partition the cloud collector still sees its three local cloud
# services (3/7) and loses only the four edge services, while the edge collector
# keeps all seven. "Cloud sees 0/7" is therefore NOT guaranteed by the topology;
# it is a measured outcome of the cut. This fixes the review's central objection
# to the old single-host Internet tier.
#
# Two modes, chosen by CLOUD_MODE:
#   local  (single-node Rust, default when PLATFORM=single-node): the harness runs
#          BOTH halves on this host. The four edge services send to the cloud over
#          HTTP (--http-cloud, the cuttable path); the three cloud services send to
#          the cloud over raw TCP (--cloud, not cut). The cut drops the HTTP cloud
#          port, so the cloud keeps 3/7 over TCP during the cut. Both collectors are
#          read from their own logs.
#   poll   (k3s | openstack | cranecloud): the cloud half (3 services + cloud
#          collector) is already deployed remotely at CLOUD_URL; the harness runs
#          the edge half locally, injects, cuts with CUT_CMD, and reads the cloud
#          collector over GET $CLOUD_URL/status.
#
# Usage examples (see RUNBOOK.md for the full per-platform steps):
#   PLATFORM=single-node EVENTS=30 scripts/e6/run_events.sh
#   PLATFORM=k3s      CLOUD_URL=http://<cloud-np>:8080 CUT_CMD='scripts/e6/cut_k3s.sh __PSEC__'       EVENTS=30 scripts/e6/run_events.sh
#   PLATFORM=openstack CLOUD_URL=http://<cloud-vm>:8080 CUT_CMD='scripts/e6/cut_openstack.sh __PSEC__' EVENTS=30 scripts/e6/run_events.sh
#   PLATFORM=cranecloud CLOUD_URL=http://<app>.cranecloud.io CUT_CMD='sudo scripts/e6/cut_cranecloud.sh __CLOUD__ __PSEC__' EVENTS=30 scripts/e6/run_events.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"

PLATFORM="${PLATFORM:?set PLATFORM (single-node|k3s|openstack|cranecloud)}"
EVENTS="${EVENTS:-30}"
BASELINE_WAIT="${BASELINE_WAIT:-12}"
OBSERVE_AFTER="${OBSERVE_AFTER:-15}"      # seconds to keep watching after the cut heals
PERIOD_MS="${PERIOD_MS:-200}"
STALE_MS="${STALE_MS:-3000}"
SEED="${SEED:-42}"; RANDOM=$SEED
read -ra PART_CHOICES <<< "${CUT_SECS:-10 30 60 120}"   # cut durations (review asks for 10/30/60/120)
MEASURE_BYTES="${MEASURE_BYTES:-0}"       # 1 = count telemetry bytes to the cloud (needs root; see bytes_counter.sh)

# Default mode by platform.
if [ "$PLATFORM" = "single-node" ]; then
  CLOUD_MODE="${CLOUD_MODE:-local}"
  CLOUD_URL="${CLOUD_URL:-http://127.0.0.1:8080}"
else
  CLOUD_MODE="${CLOUD_MODE:-poll}"
  CLOUD_URL="${CLOUD_URL:?set CLOUD_URL (the deployed cloud collector, http://host:port)}"
fi
CLOUD_URL="${CLOUD_URL%/}"
CLOUD_HOST="$(printf '%s' "$CLOUD_URL" | sed -E 's#^https?://([^:/]+).*#\1#')"
CLOUD_PORT="$(printf '%s' "$CLOUD_URL" | sed -E 's#^https?://[^:/]+:?([0-9]*).*#\1#')"
[ -n "$CLOUD_PORT" ] || CLOUD_PORT=80

# Default cut per platform (override with CUT_CMD). __PSEC__ -> duration seconds,
# __CLOUD__ -> the cloud URL. HEAL_CMD defaults to true because every shipped cut
# self-heals after its window. For every remote tier the cloud half is genuinely
# remote, so the default cut is a REAL route black-hole on the harness host to the
# cloud endpoint (scripts/cranecloud/partition_link.sh) -- a network-level cut, not
# an in-process toggle. If instead you deploy BOTH halves inside the platform, set
# CUT_CMD to the in-cluster cut: scripts/e6/cut_k3s.sh (NetworkPolicy) or
# scripts/e6/cut_openstack.sh (security group / iptables on the cloud VM).
case "$PLATFORM" in
  single-node)          DEF_CUT="sudo scripts/e6/cut_iptables.sh $CLOUD_PORT __PSEC__" ;;
  k3s|openstack|cranecloud) DEF_CUT="sudo scripts/cranecloud/partition_link.sh $CLOUD_URL 0 __PSEC__" ;;
  *)                    DEF_CUT="echo 'set CUT_CMD for $PLATFORM'; false" ;;
esac
CUT_CMD="${CUT_CMD:-$DEF_CUT}"
HEAL_CMD="${HEAL_CMD:-true}"

BIN="target/release/mesh"
[ -x "$BIN" ] || { echo "Build first: cargo build --release -p mesh"; exit 1; }

OUT="results/production/${PLATFORM}.csv"
mkdir -p results/production logs
echo "platform,event,culprit,culprit_side,partition_sec,edge_localized,edge_ttd_ms,cloud_localized_during_cut,cloud_min_visible,cross_bytes" > "$OUT"

EDGE_SVCS=(gateway ingest validate device-registry)
CLOUD_SVCS=(enrich store alerting)
ALL=("${EDGE_SVCS[@]}" "${CLOUD_SVCS[@]}")
side_of(){ for e in "${EDGE_SVCS[@]}"; do [ "$e" = "$1" ] && { echo edge; return; }; done; echo cloud; }
is_edge(){ for e in "${EDGE_SVCS[@]}"; do [ "$e" = "$1" ] && return 0; done; return 1; }
# Co-located single-node can fault any of the seven (all are local and injectable).
# On a remote tier only the edge half runs here, so faults are injected into edge
# services; the cloud half runs on the platform and keeps the cloud collector at 3/7
# during a cut. (The symmetric cloud-fault cost is shown in the simulation, E1.)
if [ "$CLOUD_MODE" = "local" ]; then POOL=("${ALL[@]}"); else POOL=("${EDGE_SVCS[@]}"); fi

kill_all(){ pkill -f "$BIN service --name" 2>/dev/null || true
            pkill -f "$BIN collector --name" 2>/dev/null || true; }

cleanup(){ kill_all; }
trap cleanup EXIT

echo "E6 on $PLATFORM: $EVENTS events, cuts from {${PART_CHOICES[*]}}s, mode=$CLOUD_MODE, cloud=$CLOUD_URL" >&2

for ev in $(seq 1 "$EVENTS"); do
  culprit="${POOL[$((RANDOM % ${#POOL[@]}))]}"; side="$(side_of "$culprit")"
  psec="${PART_CHOICES[$((RANDOM % ${#PART_CHOICES[@]}))]}"
  ts="$(date +%Y%m%d_%H%M%S)"
  elog="logs/e6_${PLATFORM}_ev${ev}_${culprit}_${ts}.edge"
  clog="logs/e6_${PLATFORM}_ev${ev}_${culprit}_${ts}.cloud"
  ccsv="results/production/${PLATFORM}_ev${ev}_${ts}.cloud.csv"
  echo "event $ev/$EVENTS culprit=$culprit($side) partition=${psec}s" >&2

  kill_all; sleep 0.4
  pids=()

  # ---- edge collector (always local) --------------------------------------
  "$BIN" collector --name edge --port 7001 --control-port 9201 --stale-ms "$STALE_MS" >"$elog" 2>&1 & pids+=($!)
  sleep 0.4

  if [ "$CLOUD_MODE" = "local" ]; then
    # cloud collector: TCP 7000 (cloud services, not cut) + HTTP 8080 (edge services, cut)
    "$BIN" collector --name cloud --port 7000 --http-port "$CLOUD_PORT" --control-port 9200 --stale-ms "$STALE_MS" >"$clog" 2>&1 & pids+=($!)
    sleep 0.4
  else
    # remote cloud: record its live view for the whole window. Default polls
    # $CLOUD_URL/status, which works when the harness reaches the cloud over a path
    # the cut does NOT sever. When the cut black-holes this host's route to the cloud
    # (so a poll from here cannot see the dip), set CLOUD_PROBE_CMD to read /status
    # from a vantage the cut leaves intact, e.g.
    #   k3s:       CLOUD_PROBE_CMD='kubectl exec deploy/cloud-collector -- wget -qO- localhost:8080/status'
    #   openstack: CLOUD_PROBE_CMD='ssh ubuntu@<cloud-ip> curl -s localhost:8080/status'
    # Cranecloud has no exec path, so leave it unset and read the 3/7 dip from the
    # Crane app logs (RUNBOOK.md), with cloud columns NA here.
    win="$((psec+BASELINE_WAIT+OBSERVE_AFTER))"
    if [ -n "${CLOUD_PROBE_CMD:-}" ]; then
      ( echo "unix_ts,http_code,visible,top1,localized_ms,t_rel_s" > "$ccsv"; t0=$SECONDS
        while [ $((SECONDS-t0)) -lt "$win" ]; do
          j="$(eval "${CLOUD_PROBE_CMD//__CLOUD__/$CLOUD_URL}" 2>/dev/null)"
          v="$(printf '%s' "$j" | grep -oE '"visible":[0-9]+' | grep -oE '[0-9]+' | head -1)"
          l="$(printf '%s' "$j" | grep -oE '"localized_ms":[0-9]+' | grep -oE '[0-9]+' | head -1)"
          echo "$(date +%s),$([ -n "$v" ] && echo 200 || echo 000),${v},,${l},$((SECONDS-t0))" >> "$ccsv"
          sleep 1
        done ) >/dev/null 2>&1 & pids+=($!)
    else
      "$ROOT/scripts/cranecloud/poll_cloud_status.sh" "$CLOUD_URL" "$win" "$ccsv" >/dev/null 2>&1 & pids+=($!)
    fi
  fi

  # ---- services: edge half uses the cut HTTP cloud path; cloud half uses TCP --
  i=0
  for s in "${ALL[@]}"; do
    cp=$((9110+i)); i=$((i+1))
    args=(service --name "$s" --culprit none --control-port "$cp" --period-ms "$PERIOD_MS" --edge 127.0.0.1:7001)
    if [ "$CLOUD_MODE" = "local" ]; then
      if is_edge "$s"; then args+=(--http-cloud "$CLOUD_URL"); else args+=(--cloud 127.0.0.1:7000); fi
      "$BIN" "${args[@]}" >"logs/e6_${PLATFORM}_$s.log" 2>&1 & pids+=($!)
    else
      # remote split: only the EDGE half runs here; the cloud half is deployed on the platform
      is_edge "$s" || continue
      args+=(--http-cloud "$CLOUD_URL")
      "$BIN" "${args[@]}" >"logs/e6_${PLATFORM}_$s.log" 2>&1 & pids+=($!)
    fi
  done

  # ---- baseline, optional byte counter, inject ----------------------------
  sleep "$BASELINE_WAIT"
  [ "$MEASURE_BYTES" = "1" ] && scripts/e6/bytes_counter.sh start "$CLOUD_HOST" "$CLOUD_PORT" 2>/dev/null || true

  # inject at the edge services' control ports (always local); for local mode also
  # hit the cloud services and the cloud collector control, so both collectors know
  # the expected culprit and report LOCALIZATION_TIME_MS when they localize it.
  if [ "$CLOUD_MODE" = "local" ]; then
    inj_services="127.0.0.1:9110,127.0.0.1:9111,127.0.0.1:9112,127.0.0.1:9113,127.0.0.1:9114,127.0.0.1:9115,127.0.0.1:9116"
    inj=(inject --culprit "$culprit" --services "$inj_services" --collectors 127.0.0.1:9201,127.0.0.1:9200)
  else
    inj_services="127.0.0.1:9110,127.0.0.1:9111,127.0.0.1:9112,127.0.0.1:9113"
    inj=(inject --culprit "$culprit" --services "$inj_services" --collectors 127.0.0.1:9201 --http-collectors "$CLOUD_URL")
  fi
  "$BIN" "${inj[@]}" >>"logs/e6_${PLATFORM}_inject.log" 2>&1 || echo "  inject failed (see logs/e6_${PLATFORM}_inject.log)" >&2

  # ---- cut the edge->cloud link for psec, then observe ---------------------
  cut="${CUT_CMD//__PSEC__/$psec}"; cut="${cut//__CLOUD__/$CLOUD_URL}"
  eval "$cut" >/dev/null 2>&1 || echo "  cut command failed; run CUT_CMD manually for $PLATFORM" >&2
  eval "${HEAL_CMD//__CLOUD__/$CLOUD_URL}" >/dev/null 2>&1 || true
  sleep "$OBSERVE_AFTER"

  cross_bytes="NA"
  [ "$MEASURE_BYTES" = "1" ] && cross_bytes="$(scripts/e6/bytes_counter.sh read "$CLOUD_HOST" "$CLOUD_PORT" 2>/dev/null || echo NA)"

  kill_all; sleep 0.3

  # ---- read the result for this event -------------------------------------
  # edge: time to diagnosis is the collector's own LOCALIZATION_TIME_MS, printed only
  # after it holds the (injected) culprit at rank 1. edge_localized follows from it.
  edge_ttd=$(grep -oE "LOCALIZATION_TIME_MS [0-9]+" "$elog" 2>/dev/null | grep -oE '[0-9]+' | head -1)
  edge_loc=$([ -n "${edge_ttd:-}" ] && echo 1 || echo 0)
  if [ "$CLOUD_MODE" = "local" ]; then
    # cloud localized the culprit at all (true for a cloud-side fault it still sees,
    # false for an edge-side fault while the edge telemetry is cut).
    cloud_ttd=$(grep -oE "LOCALIZATION_TIME_MS [0-9]+" "$clog" 2>/dev/null | grep -oE '[0-9]+' | head -1)
    cloud_loc=$([ -n "${cloud_ttd:-}" ] && echo 1 || echo 0)
    # min services the cloud saw once past the baseline window (so the startup 0/7
    # ramp is excluded): during an edge-fault cut this dips to 3/7, not 0/7.
    cloud_minvis=$(awk -v bw="$BASELINE_WAIT" '
      /t\+/ && /visible=/ {
        tr=""; v="";
        for (i=1;i<=NF;i++){
          if ($i ~ /^[0-9.]+s$/){ tr=$i; sub(/s$/,"",tr) }
          if ($i ~ /^visible=/){ v=$i; sub(/visible=/,"",v); sub(/\/.*/,"",v) }
        }
        if (tr!="" && v!="" && tr+0>=bw+0){ if (m=="" || v+0<m) m=v+0 }
      }
      END { print (m==""?"NA":m) }' "$clog" 2>/dev/null)
  else
    cloud_loc=$(awk -F, 'NR>1 && $5!="" && $5!="null"{c=1} END{print c+0}' "$ccsv" 2>/dev/null)
    cloud_minvis=$(awk -F, -v bw="$BASELINE_WAIT" 'NR>1 && $6!="" && $6+0>=bw && $3!=""{if(m==""||$3+0<m)m=$3+0} END{print (m==""?"NA":m)}' "$ccsv" 2>/dev/null)
  fi
  echo "${PLATFORM},${ev},${culprit},${side},${psec},${edge_loc},${edge_ttd:-NA},${cloud_loc:-0},${cloud_minvis:-NA},${cross_bytes}" >> "$OUT"
done

kill_all
echo "Done. $EVENTS events in $OUT"
echo "Compare with the simulation: scripts/e6/compare_prod.sh $OUT"
