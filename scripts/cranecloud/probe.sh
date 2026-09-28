#!/usr/bin/env bash
# Step 0 connectivity probe. Before running the experiment, confirm the deployed
# Crane Cloud collector is reachable from THIS machine over the real Internet.
# It checks the HTTP ingress (health + a throwaway telemetry POST) and, for the
# record, whether a raw TCP connection to the host is possible at all.
#
# Usage:
#   ./scripts/cranecloud/probe.sh <public-url>
#   e.g. ./scripts/cranecloud/probe.sh https://sovloc-xxxx.cranecloud.io
set -uo pipefail
URL="${1:?usage: probe.sh <public-url e.g. https://sovloc-xxxx.cranecloud.io>}"
URL="${URL%/}"

echo "== HTTP health =="
if curl -fsS -m 10 "$URL/healthz" >/tmp/sovloc_hz 2>/dev/null; then
  echo "  GET /healthz -> $(cat /tmp/sovloc_hz)  (OK)"
else
  echo "  GET /healthz -> FAILED (is the app deployed and the port set to 8080?)"
fi

echo "== HTTP telemetry POST (throwaway sample) =="
code=$(curl -s -o /dev/null -m 10 -w '%{http_code}' -X POST "$URL/telemetry" \
  -H 'Content-Type: application/json' -d '{"service":"store","value":1.0,"ts_ms":1}')
echo "  POST /telemetry -> HTTP $code  (expect 204)"

# Informational: managed ingresses normally answer 443/80 only; a custom raw TCP
# port usually will NOT connect. This just records what the platform exposes.
host="$(printf '%s' "$URL" | sed -E 's#^https?://##; s#/.*$##; s#:.*$##')"
echo "== raw TCP reachability to $host (informational) =="
for p in 443 80 7000; do
  if timeout 5 bash -c "exec 3<>/dev/tcp/$host/$p" 2>/dev/null; then
    echo "  tcp/$p: open"
  else
    echo "  tcp/$p: closed or filtered"
  fi
done

echo
echo "Verdict: if /healthz is OK and POST /telemetry returned 204, the collector"
echo "is reachable over HTTP across the real Internet -> proceed to run_edge_local.sh."
echo "If only https:// works (http:// refused), tell the maintainer: the local"
echo "client needs the one-line TLS build (a local rebuild only; no re-push)."
