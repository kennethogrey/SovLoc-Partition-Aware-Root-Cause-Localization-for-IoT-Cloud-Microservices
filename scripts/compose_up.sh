#!/usr/bin/env bash
# Bring up the Docker mesh with a shared clock zero, so LOCALIZATION_TIME_MS is
# meaningful across containers. Usage: ./scripts/compose_up.sh [culprit]
#
# The image is built first on purpose. The shared clock zero is only a few
# seconds ahead of now, so if we built during `up` a slow first build would push
# the real container start far past the fault time and break the shared clock.
# Building first keeps the offset honest whether or not the image is cached.
set -euo pipefail

echo "Building the mesh image (the first build compiles Rust inside the container)..."
docker compose build

export SOVLOC_EPOCH=$(( $(date +%s%3N) + 8000 ))   # 8s in the future
export SOVLOC_CULPRIT="${1:-validate}"
echo "Shared epoch = $SOVLOC_EPOCH   culprit = $SOVLOC_CULPRIT"
echo "In another terminal now run: ./scripts/partition_docker.sh 40"
docker compose up
