#!/usr/bin/env bash
# THE one command for this midterm. The only thing you need on the host is Docker
# with Compose v2. From a clean clone:
#
#     ./run.sh
#
# It builds the reproduction image and runs the whole reproduction inside a
# container: build the Rust workspace, run the unit tests, run the seeded
# experiment, then compute the confidence intervals and the figures. The results
# are written back to ./results on the host, so nothing is left inside the image.
#
# The one thing this does not do is the single uncontrolled real degraded link
# trial, which needs a real weak connection that cannot be scripted. That is
# written up as a limitation and is kept outside this repository.
set -euo pipefail
cd "$(dirname "$0")"

IMAGE=sovloc-reproduce

echo "==> Building the reproduction image (the first build compiles Rust inside Docker)..."
docker build --target reproduce -t "$IMAGE" .

echo "==> Running the reproduction inside the container..."
mkdir -p results
docker run --rm -v "$PWD/results:/app/results" "$IMAGE"
