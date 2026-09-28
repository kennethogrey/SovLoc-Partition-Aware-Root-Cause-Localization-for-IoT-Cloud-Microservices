#!/usr/bin/env bash
# Capture a real network trace to the Crane Cloud endpoint for the duration of a
# trial: ICMP round-trip time and loss (ping) plus HTTP round-trip time to
# /healthz (curl), both timestamped. Run this on the SAME uplink used for the
# trial, alongside run_edge_local.sh, so the log covers the real (and, for the
# degraded trial, uncontrolled) link conditions the exam asks to be recorded.
#
# Usage:
#   ./scripts/cranecloud/capture_link_trace.sh <public-url> [seconds]
#   e.g. ./scripts/cranecloud/capture_link_trace.sh https://sovloc-xxxx.cranecloud.io 180
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

URL="${1:?usage: capture_link_trace.sh <public-url> [seconds]}"
URL="${URL%/}"
SECONDS_TOTAL="${2:-180}"
HOST="$(printf '%s' "$URL" | sed -E 's#^https?://##; s#/.*$##; s#:.*$##')"

TS="$(date +%Y%m%d_%H%M%S)"
mkdir -p results
PING_LOG="results/realnet_linktrace_${TS}.ping.log"
HTTP_CSV="results/realnet_linktrace_${TS}.http.csv"

echo "Tracing $HOST for ${SECONDS_TOTAL}s"
echo "  ping -> $PING_LOG"
echo "  http -> $HTTP_CSV"

# Continuous ping with timestamps (RTT + loss over time).
( ping -O -D "$HOST" > "$PING_LOG" 2>&1 ) & PING_PID=$!

# HTTP round-trip time to /healthz once per second.
echo "unix_ts,http_code,time_total_s" > "$HTTP_CSV"
END=$(( $(date +%s) + SECONDS_TOTAL ))
while [ "$(date +%s)" -lt "$END" ]; do
  now=$(date +%s)
  read -r code t < <(curl -s -o /dev/null -m 5 -w '%{http_code} %{time_total}' "$URL/healthz" 2>/dev/null || echo "000 0")
  echo "${now},${code},${t}" >> "$HTTP_CSV"
  sleep 1
done

kill "$PING_PID" 2>/dev/null || true
echo
echo "Done. Summary:"
if command -v tail >/dev/null; then tail -n 3 "$PING_LOG" 2>/dev/null; fi
echo "Saved: $PING_LOG  and  $HTTP_CSV"
