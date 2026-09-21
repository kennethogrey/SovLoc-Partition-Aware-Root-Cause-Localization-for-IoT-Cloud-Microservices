#!/usr/bin/env bash
# Launch the full real mesh locally (Linux/macOS): 2 collectors + 7 services.
# Usage (from sovloc/, after `cargo build --release -p mesh`):
#   ./scripts/run_mesh.sh [culprit] [run_seconds]
set -euo pipefail

CULPRIT="${1:-validate}"
RUN_SECONDS="${2:-75}"
FAULT_AT=15
PART_AFTER=20
PART_DUR=40

BIN="target/release/mesh"
[ -x "$BIN" ] || { echo "Build first: cargo build --release -p mesh"; exit 1; }

EPOCH=$(( $(date +%s%3N) + 3000 ))
mkdir -p logs
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

"$BIN" collector --name cloud --port 7000 --expect-culprit "$CULPRIT" --start-epoch-ms "$EPOCH" --fault-at-sec "$FAULT_AT" >logs/cloud.log 2>&1 & PIDS+=($!)
"$BIN" collector --name edge  --port 7001 --expect-culprit "$CULPRIT" --start-epoch-ms "$EPOCH" --fault-at-sec "$FAULT_AT" >logs/edge.log  2>&1 & PIDS+=($!)
sleep 0.7

for s in gateway ingest validate device-registry; do
  "$BIN" service --name "$s" --culprit "$CULPRIT" --start-epoch-ms "$EPOCH" --fault-at-sec "$FAULT_AT" \
    --cloud 127.0.0.1:7000 --edge 127.0.0.1:7001 \
    --drop-cloud-after-sec "$PART_AFTER" --drop-cloud-dur-sec "$PART_DUR" >"logs/$s.log" 2>&1 & PIDS+=($!)
done
for s in enrich store alerting; do
  "$BIN" service --name "$s" --culprit "$CULPRIT" --start-epoch-ms "$EPOCH" --fault-at-sec "$FAULT_AT" \
    --cloud 127.0.0.1:7000 --edge 127.0.0.1:7001 >"logs/$s.log" 2>&1 & PIDS+=($!)
done

echo "Running ${RUN_SECONDS}s. culprit=$CULPRIT, partition ${PART_AFTER}-$((PART_AFTER+PART_DUR))s."
sleep "$RUN_SECONDS"
cleanup
echo; echo "===== CLOUD collector ====="; cat logs/cloud.log
echo; echo "===== EDGE collector  ====="; cat logs/edge.log
