#!/bin/sh
# Default entrypoint for the runtime image.
#
# With NO arguments the image starts the cloud collector on the platform's HTTP
# port, so it "just works" when deployed on Crane Cloud as a single application
# with the entry command left blank. This avoids depending on the platform
# passing a multi-word entry command through to the binary (which crash-loops the
# container into `mesh` with no subcommand if it does not).
#
# With arguments, they are passed straight to the mesh binary unchanged, so
# docker-compose (`command: ["collector", ...]`, `["service", ...]`) and any
# `docker run <image> <subcommand> ...` keep working exactly as before.
#
# Configuration via environment variables (all optional):
#   PORT / HTTP_PORT   HTTP ingress port to listen on   (default 8080)
#   SOVLOC_NAME        collector label in the logs       (default cloud)
#   SOVLOC_TCP_PORT    raw-TCP telemetry port to bind    (default 7000)
#   SOVLOC_STALE_MS    staleness window in ms            (default 3000)
set -e

HTTP_PORT="${PORT:-${HTTP_PORT:-8080}}"

# cloud-half: the cloud collector PLUS the three cloud microservices (enrich, store,
# alerting) in one container, so the E6 "cloud half" is genuinely remote on a
# platform that exposes a single app/port (Crane Cloud, or one docker run on an
# OpenStack cloud VM). The cloud services report to the local cloud collector, so
# an external edge->cloud cut leaves the cloud collector still seeing 3/7 (its three
# local services), exactly as the simulation models. Used as `... cloud-half` or
# with SOVLOC_ROLE=cloud-half.
if [ "${1:-}" = "cloud-half" ] || [ "${SOVLOC_ROLE:-}" = "cloud-half" ]; then
  C="http://127.0.0.1:$HTTP_PORT"
  # Optional scenario: with SOVLOC_SCENARIO the cloud collector builds the full
  # multi-edge call graph (e.g. the 103-service five-edge-100 topology baked into
  # /app/scenarios) so this tier matches the single-node, k3s and OpenStack tiers,
  # and the three cloud services are resolved from it. Unset = the built-in seven.
  SCEN=""
  [ -n "${SOVLOC_SCENARIO:-}" ] && SCEN="--scenario ${SOVLOC_SCENARIO}"
  mesh service --name enrich   --culprit none --control-port 9114 --period-ms "${PERIOD_MS:-200}" $SCEN --http-cloud "$C" &
  mesh service --name store    --culprit none --control-port 9115 --period-ms "${PERIOD_MS:-200}" $SCEN --http-cloud "$C" &
  mesh service --name alerting --culprit none --control-port 9116 --period-ms "${PERIOD_MS:-200}" $SCEN --http-cloud "$C" &
  exec mesh collector \
    --name "${SOVLOC_NAME:-cloud}" \
    --http-port "$HTTP_PORT" \
    --port "${SOVLOC_TCP_PORT:-7000}" \
    --control-port 9200 \
    $SCEN \
    --stale-ms "${SOVLOC_STALE_MS:-3000}"
fi

if [ "$#" -gt 0 ]; then
  exec mesh "$@"
fi

exec mesh collector \
  --name "${SOVLOC_NAME:-cloud}" \
  --http-port "$HTTP_PORT" \
  --port "${SOVLOC_TCP_PORT:-7000}" \
  --stale-ms "${SOVLOC_STALE_MS:-3000}"
