#!/usr/bin/env bash
# Run the LOCAL edge side of SovLoc against a cloud collector deployed on Crane
# Cloud, so telemetry crosses the real Internet.
#
#   local machine (7 edge services + edge collector)  <-- real Internet -->  Crane Cloud (cloud collector)
#
# The edge collector runs here over raw TCP (local, fast). Each of the 7 services
# streams every sample to BOTH the local edge collector (--edge, TCP) and the
# remote Crane Cloud collector (--http-cloud, HTTP). After the baseline freezes,
# a fault is injected into one service and both collectors are told to expect it
# (edge over TCP, cloud over HTTP). You then compare the two collectors: the edge
# keeps localizing; when the real uplink is cut/degraded the cloud goes blind.
#
# Usage:
#   ./scripts/cranecloud/run_edge_local.sh <crane-http-url> [culprit] [run_seconds]
#   e.g. ./scripts/cranecloud/run_edge_local.sh http://sovloc-xxxx.cranecloud.io validate 120
#
# Env overrides: STALE_MS (default 3000, must match the cloud collector's
# --stale-ms), PERIOD_MS (200), BASELINE_WAIT (12).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

CRANE_URL="${1:?usage: run_edge_local.sh <crane-http-url> [culprit] [run_seconds]}"
CRANE_URL="${CRANE_URL%/}"
CULPRIT="${2:-validate}"
RUN_SECONDS="${3:-120}"
STALE_MS="${STALE_MS:-3000}"
PERIOD_MS="${PERIOD_MS:-200}"
BASELINE_WAIT="${BASELINE_WAIT:-12}"

case "$CRANE_URL" in
  https://*) echo "WARNING: --http-cloud needs a plain http:// URL (the platform terminates TLS at its"
             echo "         ingress). If the cloud side shows visible=0/7 the whole run, the local client"
             echo "         could not reach https:// without TLS; see scripts/cranecloud/README.md." ;;
esac

BIN="target/release/mesh"
[ -x "$BIN" ] || { echo "Build first: cargo build --release -p mesh"; exit 1; }

# Guard: clear any stray edge services/collector from an earlier run that could
# still be streaming telemetry to the same ports and the same cloud, which would
# mix a healthy and a faulted copy of a service and corrupt the localization.
pkill -f "$BIN service --name" 2>/dev/null || true
pkill -f "$BIN collector --name edge" 2>/dev/null || true
sleep 0.5

TS="$(date +%Y%m%d_%H%M%S)"
mkdir -p results logs
EDGE_LOG="results/realnet_edge_${CULPRIT}_${TS}.log"
CLOUD_CSV="results/realnet_cloud_status_${CULPRIT}_${TS}.csv"

PIDS=()
cleanup(){ for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

echo "Cloud collector (Crane Cloud): $CRANE_URL"
echo "Edge collector (local): 127.0.0.1:7001   stale-ms=$STALE_MS   culprit=$CULPRIT"

# Capture the CLOUD collector's live view to a file by polling its GET /status
# for the whole run, so the cloud side of the result is recorded from this
# machine without scraping the platform console.
"$ROOT/scripts/cranecloud/poll_cloud_status.sh" "$CRANE_URL" "$RUN_SECONDS" "$CLOUD_CSV" >/dev/null 2>&1 &
PIDS+=($!)

# Local edge collector (raw TCP telemetry + TCP control).
"$BIN" collector --name edge --port 7001 --control-port 9201 --stale-ms "$STALE_MS" \
  >"$EDGE_LOG" 2>&1 & PIDS+=($!)
sleep 0.6

# The seven services stream to the local edge collector AND the Crane Cloud
# collector. Each gets a distinct local control port so inject can reach it.
i=0
for s in gateway ingest validate device-registry enrich store alerting; do
  cp=$((9110+i)); i=$((i+1))
  "$BIN" service --name "$s" --culprit none --control-port "$cp" --period-ms "$PERIOD_MS" \
     --edge 127.0.0.1:7001 --http-cloud "$CRANE_URL" >"logs/realnet_$s.log" 2>&1 & PIDS+=($!)
done

echo "Healthy for ${BASELINE_WAIT}s while both collectors freeze a clean baseline..."
sleep "$BASELINE_WAIT"

echo "Injecting culprit=$CULPRIT (services+edge over TCP, cloud over HTTP)..."
"$BIN" inject --culprit "$CULPRIT" \
  --services 127.0.0.1:9110,127.0.0.1:9111,127.0.0.1:9112,127.0.0.1:9113,127.0.0.1:9114,127.0.0.1:9115,127.0.0.1:9116 \
  --collectors 127.0.0.1:9201 \
  --http-collectors "$CRANE_URL"

REMAIN=$(( RUN_SECONDS - BASELINE_WAIT ))
[ "$REMAIN" -lt 10 ] && REMAIN=10
echo "Observing ${REMAIN}s. To run the partition trial, cut/degrade this machine's"
echo "uplink now (e.g. switch to a weak mobile hotspot, or block the Crane Cloud host)."
sleep "$REMAIN"
cleanup

echo
echo "===== EDGE collector (local) — key lines ====="
grep -E "now expecting|top-1 = ${CULPRIT}|LOCALIZATION|culprit\(${CULPRIT}\)=rank 1|visible=" "$EDGE_LOG" | tail -20

echo
echo "===== CLOUD collector (Crane Cloud, via GET /status) — summary ====="
awk -F, 'NR>1{
  n++;
  if($2==200) ok++;
  if($3!=""){ if($3+0>maxv) maxv=$3; if(minset==0||($3+0)<minv){minv=$3+0; minset=1} }
  if($4!="" && $4!="null") lasttop=$4;
  if($5!="" && $5!="null") loc=$5;
}
END{
  printf "  polls=%d  reachable(200)=%d\n", n, ok;
  printf "  visible: max=%s  min=%s (of 7)\n", (maxv==""?"NA":maxv), (minset?minv:"NA");
  printf "  last top-1 seen=%s   cloud localized_ms=%s\n", (lasttop==""?"NA":lasttop), (loc==""?"(not localized)":loc);
}' "$CLOUD_CSV"

echo
echo "Saved:"
echo "  edge log:          $EDGE_LOG"
echo "  cloud status CSV:  $CLOUD_CSV"
echo "Expected: edge holds ${CULPRIT} at rank 1 with visible=7/7 throughout; during a"
echo "real partition the cloud status shows visible dropping toward 0 with no top-1."
