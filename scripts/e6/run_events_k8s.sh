#!/usr/bin/env bash
# E6 production harness for the FULL multi-edge mesh on Kubernetes (k3s / a k8s on
# OpenStack / Crane Cloud if it exposes the API). It deploys the scenario mesh
# (`mesh gen-k8s`), then runs EVENTS events. Each event injects a random culprit
# and, for an edge culprit, applies a REAL per-device network cut (a NetworkPolicy
# that restricts that device's egress to its own pods plus DNS, blocking the cloud
# collector while the device's own edge collector stays reachable). It records ONE
# ROW per event to results/production/$PLATFORM.csv, the same schema as the Docker
# mesh harness, so scripts/e6/compare_prod.sh compares both to the EM simulation.
#
# Prereqs: a reachable cluster (kubectl works), and the image referenced in the
# manifest present in the cluster (push to a registry, or import locally:
#   docker build -t sovloc-mesh:local . && \
#   docker save sovloc-mesh:local | sudo k3s ctr images import -
# then gen-k8s with --image sovloc-mesh:local --image-pull-policy Never).
#
# Usage (from the repo root):
#   SCENARIO=scenarios/five-edge-100.yaml NAMESPACE=sovloc PLATFORM=k3s EVENTS=30 \
#     K8S_IMAGE=<registry>/sovloc-mesh:<tag> scripts/e6/run_events_k8s.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"

SCENARIO="${SCENARIO:?set SCENARIO=scenarios/<file>.yaml}"
PLATFORM="${PLATFORM:-k3s}"
NS="${NAMESPACE:-sovloc}"
K8S_DIR="${K8S_DIR:-deploy/k8s/generated}"
K8S_IMAGE="${K8S_IMAGE:-sovloc-mesh:local}"
PULL="${IMAGE_PULL_POLICY:-IfNotPresent}"
EVENTS="${EVENTS:-30}"
BASELINE_WAIT="${BASELINE_WAIT:-12}"
OBSERVE_AFTER="${OBSERVE_AFTER:-8}"
SETTLE="${SETTLE:-3}"
SEED="${SEED:-42}"; RANDOM=$SEED
CLOUD_HTTP="${CLOUD_HTTP:-8080}"
EDGE_HTTP_BASE="${EDGE_HTTP_BASE:-8081}"
MOUNT="${MOUNT:-/etc/sovloc/scenario.yaml}"
LEAD_MS="${LEAD_MS:-500}"
KUBECTL="${KUBECTL:-kubectl}"
KEEP_UP="${KEEP_UP:-0}"

BIN="target/release/mesh"
command -v "$KUBECTL" >/dev/null || { echo "kubectl is required"; exit 1; }
[ -x "$BIN" ] || { echo "Build first: cargo build --release -p mesh"; exit 1; }
[ -f "$SCENARIO" ] || { echo "no such scenario: $SCENARIO"; exit 1; }

kx() { "$KUBECTL" -n "$NS" "$@"; }
# run a mesh subcommand inside the control pod
ctl() { kx exec deploy/sovloc-control -- mesh "$@" 2>/dev/null; }
status_field() { # $1=host $2=httpport $3=field
  ctl status --url "http://${1}:${2}/status" | grep -oE "\"${3}\":[^,}]+" | head -1 | sed -E "s/\"${3}\"://; s/\"//g"
}

# 1) Generate and deploy the mesh.
echo "==> generating manifests ($K8S_DIR) and deploying to namespace $NS" >&2
"$BIN" gen-k8s --scenario "$SCENARIO" --out-dir "$K8S_DIR" --namespace "$NS" \
  --image "$K8S_IMAGE" --image-pull-policy "$PULL" \
  --cloud-http-port "$CLOUD_HTTP" --edge-http-base-port "$EDGE_HTTP_BASE" >/dev/null
"$KUBECTL" apply -f "$K8S_DIR/mesh.yaml" >/dev/null || { echo "kubectl apply failed"; exit 1; }

cleanup() {
  if [ "$KEEP_UP" = "1" ]; then
    echo "KEEP_UP=1: leaving the mesh deployed. Tear down with: kubectl delete -f $K8S_DIR/mesh.yaml" >&2
  else
    "$KUBECTL" delete -f "$K8S_DIR/mesh.yaml" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

echo "==> waiting for deployments to become available (up to 180s)" >&2
kx wait --for=condition=available --timeout=180s deployment --all >/dev/null 2>&1 || \
  echo "  (some deployments not ready yet; continuing)" >&2

# 2) Scenario map.
declare -A DEV_OF PSEC_OF SIDE_OF DEV_HTTP
SERVICES=(); DEVICES=(); didx=0
while IFS=, read -r svc dev psec side; do
  [ "$svc" = "service" ] && continue
  SERVICES+=("$svc"); DEV_OF[$svc]="$dev"; PSEC_OF[$svc]="$psec"; SIDE_OF[$svc]="$side"
  if [ "$side" = "edge" ] && [ -z "${DEV_HTTP[$dev]:-}" ]; then
    DEVICES+=("$dev"); DEV_HTTP[$dev]=$((EDGE_HTTP_BASE + didx)); didx=$((didx+1))
  fi
done < <("$BIN" describe --scenario "$SCENARIO")
N_SVC=${#SERVICES[@]}
echo "==> ${N_SVC} services across ${#DEVICES[@]} edge devices; waiting ${BASELINE_WAIT}s for baselines" >&2
sleep "$BASELINE_WAIT"

OUT="results/production/${PLATFORM}.csv"
mkdir -p results/production
echo "platform,event,culprit,culprit_side,device,partition_sec,edge_localized,edge_ttd_ms,cloud_localized_during_cut,cloud_min_visible,n_services" > "$OUT"

echo "==> running $EVENTS events on $PLATFORM" >&2
for ev in $(seq 1 "$EVENTS"); do
  culprit="${SERVICES[$((RANDOM % N_SVC))]}"
  side="${SIDE_OF[$culprit]}"; dev="${DEV_OF[$culprit]}"; psec="${PSEC_OF[$culprit]%.*}"
  [ "$psec" -ge 1 ] 2>/dev/null || psec=5
  echo "  event $ev/$EVENTS culprit=$culprit ($side, device=$dev, cut=${psec}s)" >&2

  edge_ttd=""; cloud_minvis=""; cloud_loc=0

  if [ "$side" = "edge" ]; then
    ehp="${DEV_HTTP[$dev]}"
    "$KUBECTL" apply -f "$K8S_DIR/cuts/cut-${dev}.yaml" >/dev/null 2>&1 || true
    ctl inject --scenario "$MOUNT" --culprit "$culprit" --lead-ms "$LEAD_MS" >/dev/null 2>&1
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt "$psec" ]; do
      lm="$(status_field "$dev" "$ehp" localized_ms)"
      [ -n "$lm" ] && [ "$lm" != "null" ] && [ -z "$edge_ttd" ] && edge_ttd="$lm"
      cv="$(status_field cloud "$CLOUD_HTTP" visible)"
      if [ -n "$cv" ]; then
        if [ -z "$cloud_minvis" ] || [ "$cv" -lt "$cloud_minvis" ]; then cloud_minvis="$cv"; fi
      fi
      cl="$(status_field cloud "$CLOUD_HTTP" localized_ms)"
      [ -n "$cl" ] && [ "$cl" != "null" ] && cloud_loc=1
      sleep 1
    done
    "$KUBECTL" delete -f "$K8S_DIR/cuts/cut-${dev}.yaml" >/dev/null 2>&1 || true
    sleep "$OBSERVE_AFTER"
    if [ -z "$edge_ttd" ]; then
      lm="$(status_field "$dev" "$ehp" localized_ms)"; [ -n "$lm" ] && [ "$lm" != "null" ] && edge_ttd="$lm"
    fi
  else
    ctl inject --scenario "$MOUNT" --culprit "$culprit" --lead-ms "$LEAD_MS" >/dev/null 2>&1
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt "$psec" ]; do
      cl="$(status_field cloud "$CLOUD_HTTP" localized_ms)"
      [ -n "$cl" ] && [ "$cl" != "null" ] && { cloud_loc=1; [ -z "$edge_ttd" ] && edge_ttd="$cl"; }
      cv="$(status_field cloud "$CLOUD_HTTP" visible)"
      if [ -n "$cv" ]; then
        if [ -z "$cloud_minvis" ] || [ "$cv" -lt "$cloud_minvis" ]; then cloud_minvis="$cv"; fi
      fi
      sleep 1
    done
    sleep "$OBSERVE_AFTER"
  fi

  edge_loc=$([ -n "$edge_ttd" ] && echo 1 || echo 0)
  echo "${PLATFORM},${ev},${culprit},${side},${dev},${psec},${edge_loc},${edge_ttd:-NA},${cloud_loc},${cloud_minvis:-NA},${N_SVC}" >> "$OUT"

  ctl inject --scenario "$MOUNT" --culprit none --lead-ms "$LEAD_MS" >/dev/null 2>&1
  sleep "$SETTLE"
done

echo "Done. $EVENTS events in $OUT" >&2
echo "Compare with the simulation: scripts/e6/compare_prod.sh $OUT" >&2
