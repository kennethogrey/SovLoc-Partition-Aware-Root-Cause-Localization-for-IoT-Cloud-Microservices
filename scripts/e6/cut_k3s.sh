#!/usr/bin/env bash
# k3s cut: apply a deny-egress NetworkPolicy that blocks the edge pods from
# reaching the cloud collector for DUR seconds, then remove it. The cloud-side
# services keep reaching the cloud collector (same node / allowed), so the cloud
# collector still sees 3/7 during the cut. Run from a host with kubectl access.
#
#   scripts/e6/cut_k3s.sh <dur_sec>
#   K8S_NS=sovloc NETPOL_FILE=deploy/k3s/partition-netpol.yaml scripts/e6/cut_k3s.sh 60
set -uo pipefail
DUR="${1:?usage: cut_k3s.sh <dur_sec>}"
NS="${K8S_NS:-default}"
NP="${NETPOL_FILE:-deploy/k3s/partition-netpol.yaml}"

cleanup() { kubectl -n "$NS" delete -f "$NP" --ignore-not-found >/dev/null 2>&1 || true; }
trap cleanup EXIT

kubectl -n "$NS" apply -f "$NP" >/dev/null
sleep "$DUR"
kubectl -n "$NS" delete -f "$NP" --ignore-not-found >/dev/null
trap - EXIT
