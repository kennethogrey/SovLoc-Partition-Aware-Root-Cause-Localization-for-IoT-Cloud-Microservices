#!/usr/bin/env bash
# Build the SovLoc runtime image (the `mesh` binary) and push it to Docker Hub so
# Crane Cloud can pull it. Run this yourself: it needs your Docker Hub login.
#
# Usage:
#   DOCKER_USER=<your-dockerhub-username> ./scripts/cranecloud/build_and_push.sh [tag]
# Example:
#   DOCKER_USER=kennethogrey ./scripts/cranecloud/build_and_push.sh v1
#
# The same image runs the cloud collector on Crane Cloud and, locally, the edge
# collector, the services, and inject. Only the entry command differs.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

: "${DOCKER_USER:?set DOCKER_USER to your Docker Hub username}"
TAG="${1:-v1}"
IMAGE="docker.io/${DOCKER_USER}/sovloc-mesh:${TAG}"

echo "==> Building the runtime image: $IMAGE"
docker build --target runtime -t "$IMAGE" .

echo "==> Pushing $IMAGE  (run 'docker login' first if you are not logged in)"
docker push "$IMAGE"

cat <<EOF

Pushed: $IMAGE

Deploy it on Crane Cloud (Create App -> Deploy single application):
  Image:          ${DOCKER_USER}/sovloc-mesh:${TAG}
  Port:           8080
  Entry command:  (LEAVE BLANK)   <- the image starts the cloud collector by itself
  Replicas:       1

The image runs the cloud collector on port 8080 by default, so no entry command
is needed. Optional env vars: SOVLOC_STALE_MS (default 3000), SOVLOC_NAME
(default cloud), PORT (default 8080).

Then open App Settings -> Domain and URLs and copy the public URL, e.g.
  https://sovloc-xxxx.cranecloud.io
and pass it to scripts/cranecloud/probe.sh and run_edge_local.sh.
EOF
