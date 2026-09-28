#!/usr/bin/env bash
# Create a REAL network partition of the edge->cloud link by black-holing the
# route to the Crane Cloud collector's IP for a window, then healing it. Only
# packets to the cloud endpoint are dropped, so the rest of the machine's
# connectivity is unaffected, and it is a genuine network-level cut (not an
# in-process toggle). Needs sudo (it changes the routing table).
#
# Run this in a SECOND terminal while run_edge_local.sh is running in the first.
# It waits so both collectors localize, cuts the cloud link, then heals it, so
# the capture shows the cloud going blind and the edge holding throughout.
#
# Usage:
#   sudo ./scripts/cranecloud/partition_link.sh <public-url> [after_sec] [dur_sec]
#   e.g. sudo ./scripts/cranecloud/partition_link.sh http://sovloc-mesh-a381b2fd.renu-01.cranecloud.io 45 60
set -euo pipefail
URL="${1:?usage: partition_link.sh <public-url> [after_sec] [dur_sec]}"
AFTER="${2:-45}"
DUR="${3:-60}"

host="$(printf '%s' "$URL" | sed -E 's#^https?://##; s#/.*$##; s#:.*$##')"
ip="$(getent hosts "$host" | awk '{print $1; exit}')"
[ -n "$ip" ] || { echo "could not resolve $host"; exit 1; }

echo "Cloud endpoint: $host = $ip"
echo "Waiting ${AFTER}s so both collectors localize before the cut..."
sleep "$AFTER"

ip route del blackhole "$ip" 2>/dev/null || true
trap 'ip route del blackhole "'"$ip"'" 2>/dev/null || true' EXIT
echo ">>> PARTITION: dropping all traffic to $ip for ${DUR}s (cloud goes blind now)"
ip route add blackhole "$ip"
sleep "$DUR"
ip route del blackhole "$ip"
trap - EXIT
echo ">>> HEALED: route to $ip restored. The edge held throughout; the cloud recovers."
