#!/usr/bin/env bash
# Poll the Crane Cloud collector's GET /status once per second and record its
# live view (visible count, top-1, localization time) to a CSV. This captures the
# cloud side of the experiment as a file from your own machine, so the result
# does not depend on scraping the platform console. run_edge_local.sh starts this
# automatically; you can also run it on its own.
#
# Usage:
#   ./scripts/cranecloud/poll_cloud_status.sh <public-url> [seconds] [outfile]
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

URL="${1:?usage: poll_cloud_status.sh <public-url> [seconds] [outfile]}"
URL="${URL%/}"
SECS="${2:-120}"
OUT="${3:-results/realnet_cloud_status_$(date +%Y%m%d_%H%M%S).csv}"
mkdir -p results

echo "unix_ts,http_code,visible,top1,localized_ms,t_rel_s" > "$OUT"
END=$(( $(date +%s) + SECS ))
while [ "$(date +%s)" -lt "$END" ]; do
  now=$(date +%s)
  body="$(curl -s -m 4 -w '\n%{http_code}' "$URL/status" 2>/dev/null || printf '\n000')"
  code="$(printf '%s' "$body" | tail -n1)"
  json="$(printf '%s' "$body" | sed '$d')"
  vis="$(printf '%s' "$json" | grep -oE '"visible":[0-9]+' | grep -oE '[0-9]+$' || true)"
  top="$(printf '%s' "$json" | grep -oE '"top1":("[^"]*"|null)' | sed 's/"top1"://; s/"//g' || true)"
  loc="$(printf '%s' "$json" | grep -oE '"localized_ms":([0-9]+|null)' | sed 's/"localized_ms"://' || true)"
  tr="$(printf '%s' "$json" | grep -oE '"t_rel_s":[0-9.]+' | sed 's/"t_rel_s"://' || true)"
  echo "${now},${code},${vis},${top},${loc},${tr}" >> "$OUT"
  sleep 1
done
echo "cloud status trace -> $OUT"
