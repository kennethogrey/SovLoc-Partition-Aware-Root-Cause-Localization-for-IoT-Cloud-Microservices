#!/usr/bin/env bash
# Inject a fault into a chosen service while the container mesh is running, or
# clear it. It runs the injector from inside the cloud container, which is already
# on the sovloc-net network and has the mesh binary, so it can reach every
# service by name.
#
# Usage:
#   ./scripts/inject.sh store     # fault the store service now, watch localization move
#   ./scripts/inject.sh gateway   # move the fault to gateway
#   ./scripts/inject.sh none      # clear the fault, back to healthy
set -euo pipefail

SERVICE="${1:?usage: ./scripts/inject.sh <service|none>}"

if [ -z "$(docker compose ps -q cloud 2>/dev/null)" ]; then
  echo "The mesh is not up. Start it first with: make demo-live"
  exit 1
fi

docker compose exec -T cloud mesh inject --culprit "$SERVICE"
echo "Injected: culprit=$SERVICE. Watch the collectors with:"
echo "  docker compose logs -f cloud edge | grep -E 'top-1|LOCALIZATION|culprit'"
