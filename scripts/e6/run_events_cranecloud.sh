#!/usr/bin/env bash
# E6 on Crane Cloud (the real-Internet tier): the SAME full multi-edge mesh as the
# other tiers (scenarios/five-edge-100.yaml, 103 services) with the cloud collector
# running on Crane Cloud (reached over HTTP, real Internet) and the 5-device/100-
# service edge half running locally. Each edge device's telemetry to the cloud goes
# out through its own source IP (10.30.0.<device>), so a partition is a REAL per-device
# cut: an iptables DROP of that device's traffic to the Crane host, while the harness
# keeps polling Crane over the host's own address. During a cut the Crane cloud loses
# exactly that device's services (103 - device size) and cannot localise its fault,
# while the device's LOCAL edge collector keeps localising. Same CSV schema as the
# Docker/k8s/OpenStack tiers, so scripts/e6/compare_prod.sh compares all four.
#
# Prereqs: the Crane app runs sovloc-mesh with SOVLOC_ROLE=cloud-half and
# SOVLOC_SCENARIO=/app/scenarios/five-edge-100.yaml (so its graph is the 103-service
# topology); CRANE_URL points at its http:// endpoint; passwordless sudo for the
# source interface, NAT and the per-device iptables cut.
#
# Usage:
#   CRANE_URL=http://sovloc-xxxx.cranecloud.io EVENTS=30 scripts/e6/run_events_cranecloud.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"

CRANE_URL="${CRANE_URL:?set CRANE_URL=http://sovloc-xxxx.cranecloud.io}"; CRANE_URL="${CRANE_URL%/}"
SCENARIO="${SCENARIO:-scenarios/five-edge-100.yaml}"
PLATFORM="${PLATFORM:-cranecloud}"
EVENTS="${EVENTS:-30}"
PERIOD_MS="${PERIOD_MS:-200}"      # same report rate as the other tiers; the free tier sustains 100 services at 200 ms
STALE_MS="${STALE_MS:-3000}"
UPLINK="${UPLINK:-wlp4s0}"         # host interface with Internet to Crane
SRCNET="${SRCNET:-10.30.0}"        # per-device source IPs SRCNET.<k>
READY_TIMEOUT="${READY_TIMEOUT:-150}"
BASELINE_SETTLE="${BASELINE_SETTLE:-24}"
OBSERVE_AFTER="${OBSERVE_AFTER:-6}"
SETTLE="${SETTLE:-3}"
SEED="${SEED:-42}"; RANDOM=$SEED
BIN="target/release/mesh"
[ -x "$BIN" ] || { echo "build: cargo build --release -p mesh"; exit 1; }
[ -f "$SCENARIO" ] || { echo "no scenario: $SCENARIO"; exit 1; }
command -v curl >/dev/null || { echo "curl required"; exit 1; }

HOST="$(printf '%s' "$CRANE_URL" | sed -E 's#^https?://##; s#[:/].*##')"
CRANE_IP="$(getent hosts "$HOST" | awk '{print $1}' | head -1)"
[ -n "$CRANE_IP" ] || { echo "cannot resolve $HOST"; exit 1; }
echo "==> Crane cloud $CRANE_URL ($CRANE_IP) via $UPLINK; per-device source $SRCNET.k" >&2

cstatus(){ curl -s -m 8 "$CRANE_URL/status" 2>/dev/null; }                    # cloud (Crane)
estatus(){ curl -s -m 5 "http://127.0.0.1:$1/status" 2>/dev/null; }           # local edge collector
field(){ printf '%s' "$1" | grep -oE "\"$2\":[^,}]+" | head -1 | sed -E "s/\"$2\"://; s/\"//g"; }

# --- scenario: edge services + devices (cloud services live on Crane) ----------
declare -A DEV_OF PSEC_OF; SERVICES=(); DEVICES=()
while IFS=, read -r svc dev psec side; do
  [ "$svc" = service ] && continue
  [ "$side" = edge ] || continue
  SERVICES+=("$svc"); DEV_OF[$svc]="$dev"; PSEC_OF[$svc]="${psec%.*}"
  case " ${DEVICES[*]} " in *" $dev "*) :;; *) DEVICES+=("$dev");; esac
done < <("$BIN" describe --scenario "$SCENARIO")
N_SVC=103   # full topology: 100 edge (local) + 3 cloud (on Crane)

# per-device: collector ports + source IP index
declare -A COL_TP COL_CP COL_HP DEV_SRC
k=0; for d in "${DEVICES[@]}"; do
  k=$((k+1))
  COL_TP[$d]=$((7000+k)); COL_CP[$d]=$((7100+k)); COL_HP[$d]=$((8100+k)); DEV_SRC[$d]="$SRCNET.$k"
done
declare -A SVC_CP; i=0
for s in "${SERVICES[@]}"; do SVC_CP[$s]=$((9000+i)); i=$((i+1)); done

# --- host source-routing: a source IP per device, NAT to the uplink ------------
echo "==> host setup: source interface + NAT" >&2
sudo ip link add cranesrc type dummy 2>/dev/null || true
for d in "${DEVICES[@]}"; do sudo ip addr add "${DEV_SRC[$d]}/24" dev cranesrc 2>/dev/null || true; done
sudo ip link set cranesrc up
sudo sysctl -q -w net.ipv4.ip_forward=1
sudo sysctl -q -w net.ipv4.conf.all.rp_filter=2 2>/dev/null || true
sudo iptables -t nat -C POSTROUTING -s "$SRCNET.0/24" -o "$UPLINK" -j MASQUERADE 2>/dev/null \
  || sudo iptables -t nat -A POSTROUTING -s "$SRCNET.0/24" -o "$UPLINK" -j MASQUERADE
cut_dev(){ sudo iptables -I OUTPUT -s "${DEV_SRC[$1]}" -d "$CRANE_IP" -j DROP; }
heal_dev(){
  sudo iptables -D OUTPUT -s "${DEV_SRC[$1]}" -d "$CRANE_IP" -j DROP 2>/dev/null || true
  # Drop the conntrack entries for this device's dropped flows so its services
  # re-establish cleanly over the real Internet instead of riding dead sockets.
  sudo conntrack -D -s "${DEV_SRC[$1]}" -d "$CRANE_IP" >/dev/null 2>&1 || true
}
# Wait for the cloud to see (almost) all services again, so the next event starts
# from a clean 103 and its cloud_min reflects only that event's cut.
recover(){
  local rr=$SECONDS t=$(( N_SVC * 96 / 100 ))
  while [ $((SECONDS - rr)) -lt "${RECOVER_TIMEOUT:-25}" ]; do
    local rv; rv="$(field "$(cstatus)" visible)"
    [ -n "$rv" ] && [ "$rv" -ge "$t" ] 2>/dev/null && break
    sleep 2
  done
}
cleanup(){
  pkill -9 -x mesh 2>/dev/null || true
  for d in "${DEVICES[@]}"; do heal_dev "$d"; done
}
trap cleanup EXIT

# --- deploy the local edge half -----------------------------------------------
pkill -9 -x mesh 2>/dev/null || true; sleep 2
echo "==> starting ${#DEVICES[@]} edge collectors + ${#SERVICES[@]} edge services (-> Crane over HTTP)" >&2
for d in "${DEVICES[@]}"; do
  "$BIN" collector --name "$d" --port "${COL_TP[$d]}" --control-port "${COL_CP[$d]}" \
    --http-port "${COL_HP[$d]}" --scenario "$SCENARIO" --stale-ms "$STALE_MS" >/tmp/crane_col_$d.log 2>&1 &
done
sleep 1
for s in "${SERVICES[@]}"; do
  d="${DEV_OF[$s]}"
  "$BIN" service --name "$s" --culprit none --scenario "$SCENARIO" \
    --edge "127.0.0.1:${COL_TP[$d]}" --http-cloud "$CRANE_URL" --http-bind "${DEV_SRC[$d]}" \
    --control-port "${SVC_CP[$s]}" --period-ms "$PERIOD_MS" >/tmp/crane_svc_${SVC_CP[$s]}.log 2>&1 &
done

# inject targets: local service + collector control ports; Crane cloud over HTTP
INJ_SVCS=""; for s in "${SERVICES[@]}"; do INJ_SVCS+="127.0.0.1:${SVC_CP[$s]},"; done; INJ_SVCS="${INJ_SVCS%,}"
INJ_COLS=""; for d in "${DEVICES[@]}"; do INJ_COLS+="127.0.0.1:${COL_CP[$d]},"; done; INJ_COLS="${INJ_COLS%,}"
inject(){ "$BIN" inject --scenario "$SCENARIO" --culprit "$1" --services "$INJ_SVCS" \
            --collectors "$INJ_COLS" --http-collectors "$CRANE_URL" >/dev/null 2>&1; }

# --- wait until Crane sees (almost) all 103 before baselines settle ------------
echo "==> waiting for services to reach Crane over the Internet" >&2
rt=$SECONDS; target=$(( N_SVC * 95 / 100 ))
while [ $((SECONDS - rt)) -lt "$READY_TIMEOUT" ]; do
  cv="$(field "$(cstatus)" visible)"
  echo "  Crane sees ${cv:-0}/$N_SVC" >&2
  [ -n "$cv" ] && [ "$cv" -ge "$target" ] 2>/dev/null && break
  sleep 6
done
sleep "$BASELINE_SETTLE"

cv="$(field "$(cstatus)" visible)"
echo "==> self-test: Crane sees ${cv:-?}/$N_SVC" >&2
[ -n "$cv" ] && [ "$cv" -ge $((N_SVC/2)) ] 2>/dev/null || { echo "ABORT: Crane cloud not healthy; no CSV written" >&2; exit 1; }

OUT="results/production/${PLATFORM}.csv"; mkdir -p results/production
echo "platform,event,culprit,culprit_side,device,partition_sec,edge_localized,edge_ttd_ms,cloud_localized_during_cut,cloud_min_visible,n_services" > "$OUT"

# --- event loop (edge culprits only: the 3 cloud services live on Crane) -------
EDGE=(); for s in "${SERVICES[@]}"; do EDGE+=("$s"); done
echo "==> running $EVENTS events on $PLATFORM" >&2
for ev in $(seq 1 "$EVENTS"); do
  culprit="${EDGE[$((RANDOM % ${#EDGE[@]}))]}"
  d="${DEV_OF[$culprit]}"; psec="${PSEC_OF[$culprit]}"; [ "$psec" -ge 1 ] 2>/dev/null || psec=10
  # Cap the cut length: over the real uncontrolled Internet link a partition longer
  # than ~40s does not cleanly re-establish all of a device's HTTP streams on heal,
  # so the cloud would keep losing that device on later events. The per-device cut
  # and the cloud-blind result are unchanged; only the longest cuts are shortened.
  [ "$psec" -gt "${PSEC_CAP:-40}" ] 2>/dev/null && psec="${PSEC_CAP:-40}"
  echo "  event $ev/$EVENTS culprit=$culprit (edge, device=$d, cut=${psec}s)" >&2
  edge_ttd=""; cloud_minvis=""; cloud_loc=0
  inject "$culprit"
  cut_dev "$d"
  t0=$SECONDS
  while [ $((SECONDS - t0)) -lt "$psec" ]; do
    lm="$(field "$(estatus "${COL_HP[$d]}")" localized_ms)"; [ -n "$lm" ] && [ "$lm" != null ] && [ -z "$edge_ttd" ] && edge_ttd="$lm"
    cs="$(cstatus)"
    cvv="$(field "$cs" visible)"; [ -n "$cvv" ] && { [ -z "$cloud_minvis" ] || [ "$cvv" -lt "$cloud_minvis" ]; } && cloud_minvis="$cvv"
    cl="$(field "$cs" localized_ms)"; [ -n "$cl" ] && [ "$cl" != null ] && cloud_loc=1
    sleep 1
  done
  heal_dev "$d"
  sleep "$OBSERVE_AFTER"
  [ -z "$edge_ttd" ] && { lm="$(field "$(estatus "${COL_HP[$d]}")" localized_ms)"; [ -n "$lm" ] && [ "$lm" != null ] && edge_ttd="$lm"; }
  edge_loc=$([ -n "$edge_ttd" ] && echo 1 || echo 0)
  echo "${PLATFORM},${ev},${culprit},edge,${d},${psec},${edge_loc},${edge_ttd:-NA},${cloud_loc},${cloud_minvis:-NA},${N_SVC}" >> "$OUT"
  inject none
  recover
  sleep "$SETTLE"
done
echo "Done. $EVENTS events in $OUT" >&2
echo "Compare: scripts/e6/compare_prod.sh $OUT" >&2
