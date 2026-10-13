#!/usr/bin/env bash
# OpenStack cut: block the edge VM from the cloud collector's port for DUR seconds
# at the cloud VM, then heal. The three cloud services run on the cloud VM and keep
# reaching the cloud collector locally, so the cloud collector still sees 3/7 during
# the cut (a genuinely remote split, not "cloud sees 0/7 by construction").
#
# Two mechanisms; pick with METHOD (default iptables on the cloud VM):
#   iptables : ssh to the cloud VM and DROP traffic from the edge VM to the port.
#   secgroup : remove/re-add an OpenStack security-group rule (needs the openstack CLI
#              and a dedicated rule id in SG_RULE_CIDR).
#
#   CLOUD_SSH=ubuntu@<cloud-fip> EDGE_IP=<edge-private-ip> CLOUD_PORT=8080 \
#     scripts/e6/cut_openstack.sh <dur_sec>
set -uo pipefail
DUR="${1:?usage: cut_openstack.sh <dur_sec>}"
METHOD="${METHOD:-iptables}"
PORT="${CLOUD_PORT:-8080}"
SSH_OPTS="${SSH_OPTS:--o StrictHostKeyChecking=no -o ConnectTimeout=8}"

if [ "$METHOD" = "secgroup" ]; then
  SG="${SG:?set SG (security group name/id guarding the cloud collector)}"
  CIDR="${SG_RULE_CIDR:?set SG_RULE_CIDR (edge CIDR, e.g. 10.0.0.0/24)}"
  cleanup() { openstack security group rule create --proto tcp --dst-port "$PORT" --remote-ip "$CIDR" "$SG" >/dev/null 2>&1 || true; }
  trap cleanup EXIT
  rid="$(openstack security group rule list "$SG" -f value -c ID -c Port\ Range 2>/dev/null | awk -v p="$PORT" '$2 ~ p {print $1; exit}')"
  [ -n "$rid" ] && openstack security group rule delete "$rid" >/dev/null
  sleep "$DUR"
  openstack security group rule create --proto tcp --dst-port "$PORT" --remote-ip "$CIDR" "$SG" >/dev/null
  trap - EXIT
else
  CLOUD_SSH="${CLOUD_SSH:?set CLOUD_SSH=user@cloud-vm}"
  EDGE_IP="${EDGE_IP:?set EDGE_IP (the edge address the cloud VM sees)}"
  rrule() { ssh $SSH_OPTS "$CLOUD_SSH" "sudo iptables -$1 INPUT -s $EDGE_IP -p tcp --dport $PORT -j DROP" 2>/dev/null; }
  cleanup() { rrule D || true; }
  trap cleanup EXIT
  rrule A
  sleep "$DUR"
  rrule D
  trap - EXIT
fi
