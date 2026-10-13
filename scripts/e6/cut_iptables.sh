#!/usr/bin/env bash
# Single-node cut: drop TCP to the cloud collector's HTTP port for DUR seconds,
# then heal. Only the edge services reach the cloud over this port, so during the
# cut the cloud collector loses the four edge services but still receives its three
# local cloud services over raw TCP (port 7000, untouched). That reproduces the
# simulation's asymmetry (cloud keeps 3/7) on one host. Needs root (iptables).
#
#   sudo scripts/e6/cut_iptables.sh <cloud_http_port> <dur_sec>
set -uo pipefail
PORT="${1:?usage: cut_iptables.sh <cloud_http_port> <dur_sec>}"
DUR="${2:?usage: cut_iptables.sh <cloud_http_port> <dur_sec>}"

rule() { # $1 = A (add) or D (delete)
  iptables -"$1" OUTPUT -o lo -p tcp --dport "$PORT" -j DROP 2>/dev/null
  iptables -"$1" INPUT  -p tcp --dport "$PORT" -j DROP 2>/dev/null
}
cleanup() { rule D || true; }
trap cleanup EXIT

rule A
sleep "$DUR"
rule D
trap - EXIT
