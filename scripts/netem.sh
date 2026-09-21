#!/usr/bin/env bash
# Apply/remove REAL network degradation (latency + loss) on a container's
# interface using tc/netem, the exam's "degraded-network" condition (§5.3).
# Requires the container to have NET_ADMIN (docker-compose.yml grants it).
#
# Usage:
#   ./scripts/netem.sh <service> add [delay_ms] [loss_pct] [rate_kbit]
#   ./scripts/netem.sh <service> del
# Example (rural-Uganda-ish link on the cloud collector):
#   ./scripts/netem.sh cloud add 250 8 512
set -euo pipefail

SVC="${1:?service name, e.g. cloud}"
OP="${2:?add|del}"
DELAY="${3:-200}"
LOSS="${4:-10}"
RATE="${5:-1024}"

CID="$(docker compose ps -q "$SVC")"
[ -n "$CID" ] || { echo "container for '$SVC' not found"; exit 1; }

if [ "$OP" = "add" ]; then
  docker exec "$CID" tc qdisc add dev eth0 root netem delay "${DELAY}ms" loss "${LOSS}%" rate "${RATE}kbit"
  echo "netem on $SVC: ${DELAY}ms delay, ${LOSS}% loss, ${RATE}kbit."
elif [ "$OP" = "del" ]; then
  docker exec "$CID" tc qdisc del dev eth0 root || true
  echo "netem cleared on $SVC."
else
  echo "unknown op: $OP (use add|del)"; exit 1
fi
