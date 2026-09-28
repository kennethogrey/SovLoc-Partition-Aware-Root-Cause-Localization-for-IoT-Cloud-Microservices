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

if [ "$#" -gt 0 ]; then
  exec mesh "$@"
fi

HTTP_PORT="${PORT:-${HTTP_PORT:-8080}}"
exec mesh collector \
  --name "${SOVLOC_NAME:-cloud}" \
  --http-port "$HTTP_PORT" \
  --port "${SOVLOC_TCP_PORT:-7000}" \
  --stale-ms "${SOVLOC_STALE_MS:-3000}"
