#!/usr/bin/env bash
# Create a REAL network partition of the cloud collector by disconnecting it
# from the container network for a duration, then healing it. This is a genuine
# partition (not an in-process toggle). Run while `compose_up.sh` is up.
#
# Usage: ./scripts/partition_docker.sh [duration_seconds]
set -euo pipefail
NET=sovloc-net
DUR="${1:-40}"

CID="$(docker compose ps -q cloud)"
[ -n "$CID" ] || { echo "cloud container not found (is the stack up?)"; exit 1; }

echo "Partitioning cloud collector from $NET for ${DUR}s..."
docker network disconnect "$NET" "$CID"
sleep "$DUR"
docker network connect "$NET" "$CID"
echo "Cloud link healed. Inspect: docker compose logs cloud edge | grep -E 'top-1|LOCALIZATION'"
