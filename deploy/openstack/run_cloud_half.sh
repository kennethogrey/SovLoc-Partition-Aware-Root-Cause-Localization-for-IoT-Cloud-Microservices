#!/usr/bin/env bash
# Run the SovLoc cloud half (cloud collector + enrich, store, alerting) in one
# container on an OpenStack cloud VM. The edge half runs on the edge VM (or the
# harness host) via scripts/e6/run_events.sh. Needs Docker on the VM.
#
#   IMAGE=<registry>/sovloc-mesh:<tag> ./deploy/openstack/run_cloud_half.sh
#
# Then open port 8080 to the edge VM in the instance's security group, and point
# the harness at CLOUD_URL=http://<this-vm-ip>:8080.
set -euo pipefail
IMAGE="${IMAGE:?set IMAGE=<registry>/sovloc-mesh:<tag> (built with scripts/cranecloud/build_and_push.sh)}"
PORT="${PORT:-8080}"
NAME="${NAME:-sovloc-cloud}"

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "${PORT}:8080" -e SOVLOC_ROLE=cloud-half "$IMAGE"
echo "cloud half running as container '$NAME' on port ${PORT}."
echo "Verify: curl -s http://127.0.0.1:${PORT}/healthz   (expect: ok)"
echo "Open ${PORT}/tcp to the edge VM in the security group, then on the edge side:"
echo "  PLATFORM=openstack CLOUD_URL=http://<this-vm-ip>:${PORT} EVENTS=30 scripts/e6/run_events.sh"
