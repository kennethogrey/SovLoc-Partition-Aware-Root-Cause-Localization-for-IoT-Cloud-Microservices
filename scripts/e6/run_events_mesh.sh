#!/usr/bin/env bash
# E6 production harness for the FULL multi-edge mesh (real per-device network
# partition). It brings up the whole scenario mesh once with `docker compose`
# (one container per microservice, one edge collector per device, a cloud
# collector, and a control helper), then runs EVENTS independent events. Each
# event injects a random culprit, applies a REAL network cut of that culprit's
# edge device from the cloud (`docker network disconnect`), and records ONE ROW
# per event to results/production/$PLATFORM.csv.
#
# The cut is genuine: the device's services can no longer reach the cloud
# collector, while their own edge collector (same docker network) and the cloud
# microservices (cloud-net) are unaffected. So during an edge-device cut the edge
# collector keeps localizing that device's fault, while the cloud collector loses
# exactly that device's services (visible drops by the device size) and cannot
# localize the edge fault until the link heals. That is the simulation's result
# (EM), now on a real container network, for a topology of 5+ edges with ~20
# microservices each and a different partition per edge.
#
# Usage (from the repo root):
#   SCENARIO=scenarios/five-edge-100.yaml PLATFORM=single-node-mesh EVENTS=30 \
#     scripts/e6/run_events_mesh.sh
#
# Env knobs: COMPOSE_FILE (default docker-compose.generated.yml, regenerated from
#   SCENARIO), BASELINE_WAIT (10s for collector baselines to freeze), OBSERVE_AFTER
#   (8s post-heal), SETTLE (3s between events), SEED (42), CLOUD_HTTP (8080),
#   EDGE_HTTP_BASE (8081), BUILD (1 => up --build), KEEP_UP (1 => leave mesh up).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"

SCENARIO="${SCENARIO:?set SCENARIO=scenarios/<file>.yaml}"
PLATFORM="${PLATFORM:-single-node-mesh}"
COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.generated.yml}"
EVENTS="${EVENTS:-30}"
BASELINE_WAIT="${BASELINE_WAIT:-10}"
OBSERVE_AFTER="${OBSERVE_AFTER:-8}"
SETTLE="${SETTLE:-3}"
SEED="${SEED:-42}"; RANDOM=$SEED
CLOUD_HTTP="${CLOUD_HTTP:-8080}"
EDGE_HTTP_BASE="${EDGE_HTTP_BASE:-8081}"
MOUNT="${MOUNT:-/app/scenario.yaml}"
LEAD_MS="${LEAD_MS:-500}"
BUILD="${BUILD:-1}"
KEEP_UP="${KEEP_UP:-0}"
# Use one prebuilt image for every container instead of a per-service `build: .`.
# The compose lists the image ~110 times, so letting Compose build it per service
# launches ~110 concurrent `cargo build` jobs. With IMAGE set, build the image once
# (docker build -t <IMAGE> .) and every service just references it. Off by default.
IMAGE="${IMAGE:-}"

# Optional B7 constrained-hardware footprint. When CAP_COLLECTORS=1 the cloud and
# each edge collector container is capped with cgroup v2 limits of CAP_CPUS vCPU
# and CAP_MEM memory (the microservice containers stay unconstrained, they only
# generate load). The run then writes a distinct CSV, appends two provenance
# columns, and tags the platform label with -capped, so the standard run and its
# results/production/<platform>.csv are left untouched. Off by default.
CAP_COLLECTORS="${CAP_COLLECTORS:-0}"
CAP_CPUS="${CAP_CPUS:-1.0}"
CAP_MEM="${CAP_MEM:-512m}"
CAP_HDR=""; CAP_ROW=""
if [ "$CAP_COLLECTORS" = "1" ]; then
  CAP_MEM_MB="${CAP_MEM%m}"
  CAP_HDR=",cap_cpus,cap_mem_mb"
  CAP_ROW=",${CAP_CPUS},${CAP_MEM_MB}"
  PLATFORM="${PLATFORM}-capped"
fi

BIN="target/release/mesh"
command -v docker >/dev/null || { echo "docker is required"; exit 1; }
command -v curl   >/dev/null || { echo "curl is required to poll /status"; exit 1; }
[ -x "$BIN" ] || { echo "Build first: cargo build --release -p mesh"; exit 1; }
[ -f "$SCENARIO" ] || { echo "no such scenario: $SCENARIO"; exit 1; }

# 1) Generate the compose for this scenario and bring the mesh up once.
echo "==> generating $COMPOSE_FILE from $SCENARIO" >&2
"$BIN" gen-compose --scenario "$SCENARIO" --out "$COMPOSE_FILE" \
  --cloud-http-port "$CLOUD_HTTP" --edge-http-base-port "$EDGE_HTTP_BASE" >/dev/null

# Point every service at one prebuilt image instead of 110 per-service builds.
if [ -n "$IMAGE" ]; then
  echo "==> using prebuilt image $IMAGE for all services (no per-service build)" >&2
  sed -i "s|^    build: \.\$|    image: ${IMAGE}|" "$COMPOSE_FILE"
  BUILD=0
fi

# B7: cap only the collector services (cloud + edge-N), never the edge-N-svc-M
# microservices. The service header lines are exactly "  cloud:" and "  edge-N:".
if [ "$CAP_COLLECTORS" = "1" ]; then
  echo "==> capping cloud + edge collectors at ${CAP_CPUS} vCPU / ${CAP_MEM} (cgroup v2)" >&2
  awk -v c="$CAP_CPUS" -v m="$CAP_MEM" '
    { print }
    /^  (cloud|edge-[0-9]+):$/ { print "    cpus: \"" c "\""; print "    mem_limit: " m }
  ' "$COMPOSE_FILE" > "$COMPOSE_FILE.capped" && mv "$COMPOSE_FILE.capped" "$COMPOSE_FILE"
fi

PROJECT="$(docker compose -f "$COMPOSE_FILE" config 2>/dev/null | awk '/^name:/{print $2; exit}')"
[ -n "$PROJECT" ] || { echo "could not read compose project name"; exit 1; }

echo "==> bringing up the mesh ($PROJECT); first run builds the image" >&2
if [ "$BUILD" = "1" ]; then
  docker compose -f "$COMPOSE_FILE" up -d --build || { echo "compose up failed"; exit 1; }
else
  docker compose -f "$COMPOSE_FILE" up -d || { echo "compose up failed"; exit 1; }
fi

cleanup() {
  if [ "$KEEP_UP" = "1" ]; then
    echo "KEEP_UP=1: leaving the mesh running ($PROJECT). Tear down with: docker compose -f $COMPOSE_FILE down" >&2
  else
    docker compose -f "$COMPOSE_FILE" down >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

CLOUD_CID="$(docker compose -f "$COMPOSE_FILE" ps -q cloud)"
[ -n "$CLOUD_CID" ] || { echo "cloud collector container not found"; exit 1; }

# 2) Scenario map: service -> device, partition, side; and device -> http port/net.
declare -A DEV_OF PSEC_OF SIDE_OF DEV_HTTP
SERVICES=(); DEVICES=()
didx=0
while IFS=, read -r svc dev psec side; do
  [ "$svc" = "service" ] && continue   # header
  SERVICES+=("$svc"); DEV_OF[$svc]="$dev"; PSEC_OF[$svc]="$psec"; SIDE_OF[$svc]="$side"
  if [ "$side" = "edge" ] && [ -z "${DEV_HTTP[$dev]:-}" ]; then
    DEVICES+=("$dev"); DEV_HTTP[$dev]=$((EDGE_HTTP_BASE + didx)); didx=$((didx+1))
  fi
done < <("$BIN" describe --scenario "$SCENARIO")
N_SVC=${#SERVICES[@]}
echo "==> ${N_SVC} services across ${#DEVICES[@]} edge devices; waiting ${BASELINE_WAIT}s for baselines" >&2
sleep "$BASELINE_WAIT"

# status_field <http_port> <field>  -> value (number or 'null'), or empty on error
status_field() {
  curl -s --max-time 2 "http://127.0.0.1:${1}/status" 2>/dev/null \
    | grep -oE "\"${2}\":[^,}]+" | head -1 | sed -E "s/\"${2}\"://; s/\"//g"
}
inject() { docker compose -f "$COMPOSE_FILE" exec -T sovloc-control \
             mesh inject --scenario "$MOUNT" --culprit "$1" --lead-ms "$LEAD_MS" >/dev/null 2>&1; }

OUT="results/production/${PLATFORM}.csv"
mkdir -p results/production
echo "platform,event,culprit,culprit_side,device,partition_sec,edge_localized,edge_ttd_ms,cloud_localized_during_cut,cloud_min_visible,n_services,cross_bytes,cpu_ms,rss_kb,p99_handler_us${CAP_HDR}" > "$OUT"

echo "==> running $EVENTS events on $PLATFORM" >&2
for ev in $(seq 1 "$EVENTS"); do
  culprit="${SERVICES[$((RANDOM % N_SVC))]}"
  side="${SIDE_OF[$culprit]}"; dev="${DEV_OF[$culprit]}"; psec="${PSEC_OF[$culprit]%.*}"
  [ "$psec" -ge 1 ] 2>/dev/null || psec=5
  echo "  event $ev/$EVENTS culprit=$culprit ($side, device=$dev, cut=${psec}s)" >&2

  edge_ttd=""; cloud_minvis=""; cloud_loc=0
  # Measured footprint of the cloud (central) collector: snapshot its received
  # bytes and CPU before the event, difference them after, so cross_bytes and
  # cpu_ms are the real cost of this event, not a model constant (E4).
  bytes0="$(status_field "$CLOUD_HTTP" bytes_in)"; [ -n "$bytes0" ] || bytes0=0
  cpu0="$(status_field "$CLOUD_HTTP" cpu_ms)"; [ -n "$cpu0" ] || cpu0=0

  if [ "$side" = "edge" ]; then
    ehp="${DEV_HTTP[$dev]}"
    # Cut first, then inject, so the fault begins while the cloud is already blind
    # to this device: the edge collector localizes it, the cloud cannot.
    docker network disconnect "${PROJECT}_${dev}-net" "$CLOUD_CID" >/dev/null 2>&1 || true
    inject "$culprit"
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt "$psec" ]; do
      lm="$(status_field "$ehp" localized_ms)"
      [ -n "$lm" ] && [ "$lm" != "null" ] && [ -z "$edge_ttd" ] && edge_ttd="$lm"
      cv="$(status_field "$CLOUD_HTTP" visible)"
      if [ -n "$cv" ]; then
        if [ -z "$cloud_minvis" ] || [ "$cv" -lt "$cloud_minvis" ]; then cloud_minvis="$cv"; fi
      fi
      cl="$(status_field "$CLOUD_HTTP" localized_ms)"
      [ -n "$cl" ] && [ "$cl" != "null" ] && cloud_loc=1
      sleep 1
    done
    docker network connect "${PROJECT}_${dev}-net" "$CLOUD_CID" >/dev/null 2>&1 || true
    sleep "$OBSERVE_AFTER"
    if [ -z "$edge_ttd" ]; then
      lm="$(status_field "$ehp" localized_ms)"; [ -n "$lm" ] && [ "$lm" != "null" ] && edge_ttd="$lm"
    fi
  else
    # Cloud-side fault: the cloud collector localizes it (its services are never
    # cut); edge collectors never receive cloud telemetry, so they stay blind.
    inject "$culprit"
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt "$psec" ]; do
      cl="$(status_field "$CLOUD_HTTP" localized_ms)"
      [ -n "$cl" ] && [ "$cl" != "null" ] && { cloud_loc=1; [ -z "$edge_ttd" ] && edge_ttd="$cl"; }
      cv="$(status_field "$CLOUD_HTTP" visible)"
      if [ -n "$cv" ]; then
        if [ -z "$cloud_minvis" ] || [ "$cv" -lt "$cloud_minvis" ]; then cloud_minvis="$cv"; fi
      fi
      sleep 1
    done
    sleep "$OBSERVE_AFTER"
  fi

  edge_loc=$([ -n "$edge_ttd" ] && echo 1 || echo 0)
  # Read the cloud collector's end-of-event footprint and difference the counters.
  bytes1="$(status_field "$CLOUD_HTTP" bytes_in)"; [ -n "$bytes1" ] || bytes1="$bytes0"
  cpu1="$(status_field "$CLOUD_HTTP" cpu_ms)"; [ -n "$cpu1" ] || cpu1="$cpu0"
  rss="$(status_field "$CLOUD_HTTP" rss_kb)"; [ -n "$rss" ] || rss=NA
  p99="$(status_field "$CLOUD_HTTP" p99_handler_us)"; [ -n "$p99" ] || p99=NA
  cross_bytes=$(( bytes1 - bytes0 )); [ "$cross_bytes" -ge 0 ] 2>/dev/null || cross_bytes=NA
  cpu_ms=$(( cpu1 - cpu0 )); [ "$cpu_ms" -ge 0 ] 2>/dev/null || cpu_ms=NA
  echo "${PLATFORM},${ev},${culprit},${side},${dev},${psec},${edge_loc},${edge_ttd:-NA},${cloud_loc},${cloud_minvis:-NA},${N_SVC},${cross_bytes},${cpu_ms},${rss},${p99}${CAP_ROW}" >> "$OUT"

  inject none      # clear the fault
  sleep "$SETTLE"  # let the faulted service settle before the next event
done

echo "Done. $EVENTS events in $OUT" >&2
echo "Compare with the simulation: scripts/e6/compare_prod.sh $OUT" >&2
