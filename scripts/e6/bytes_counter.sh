#!/usr/bin/env bash
# Measure the telemetry bytes that cross the edge->cloud boundary, using an
# iptables accounting rule (a counting rule with no target side effect) on the
# traffic this host sends to the cloud collector's port. Enable it from the E6
# harness with MEASURE_BYTES=1. Needs root. On managed platforms where the edge
# host cannot see the counter (for example the k3s cut is pod-to-pod on another
# node), leave it off and bytes stay NA for that tier, which is disclosed.
#
#   sudo scripts/e6/bytes_counter.sh start <cloud_host> <port>   # zero the baseline
#   sudo scripts/e6/bytes_counter.sh read  <cloud_host> <port>   # print bytes since start
set -uo pipefail
ACTION="${1:?start|read}"; HOST="${2:?host}"; PORT="${3:?port}"
ip="$(getent hosts "$HOST" | awk '{print $1; exit}')"; [ -n "$ip" ] || ip="$HOST"
STATE="/tmp/sovloc_bytes_${ip//[^0-9]/_}_${PORT}"

ensure() {
  iptables -C OUTPUT -d "$ip" -p tcp --dport "$PORT" -m comment --comment sovloc-bytes 2>/dev/null \
    || iptables -A OUTPUT -d "$ip" -p tcp --dport "$PORT" -m comment --comment sovloc-bytes 2>/dev/null
}
curbytes() {
  iptables -nvxL OUTPUT 2>/dev/null | awk '/sovloc-bytes/ {print $2; exit}'
}

case "$ACTION" in
  start) ensure; curbytes > "$STATE" 2>/dev/null || echo 0 > "$STATE" ;;
  read)  now="$(curbytes)"; was="$(cat "$STATE" 2>/dev/null || echo 0)"
         echo $(( ${now:-0} - ${was:-0} )) ;;
  *)     echo "usage: bytes_counter.sh start|read <host> <port>" >&2; exit 1 ;;
esac
