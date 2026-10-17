#!/usr/bin/env bash
# E6 production on OpenStack (Kolla-Ansible all-in-one): a reduced multi-edge mesh
# run across real VMs, one VM per edge device plus a cloud VM. The mesh runs as the
# static musl `mesh` binary natively on cirros guests. A partition is a REAL network
# cut inside the edge VM (an `ip route add blackhole <cloud>` that severs that
# device's telemetry to the cloud); the device's own edge collector keeps localising
# its fault, while the cloud collector loses exactly that device's services until the
# route is removed. Reached through the neutron DHCP namespace (no floating IPs).
# One row per event to results/production/openstack.csv, the same schema as the
# Docker / k8s tiers, so scripts/e6/compare_prod.sh compares all of them.
#
# Prereqs: a working Kolla OpenStack (clouds.yaml at $OS_CLIENT_CONFIG_FILE), the
# VMs cloud-vm/edge1-vm/edge2-vm on network $NET with key $KEY, and the static
# binary built: cargo build --release -p mesh --target x86_64-unknown-linux-musl
#
# Usage:
#   SCENARIO=scenarios/openstack-2edge.yaml EVENTS=30 scripts/e6/run_events_openstack.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"

SCENARIO="${SCENARIO:-scenarios/openstack-2edge.yaml}"
PLATFORM="${PLATFORM:-openstack}"
EVENTS="${EVENTS:-30}"
NET="${NET:-sovloc-net}"
KEY="${KEY:-$HOME/kolla/sovloc-key.pem}"
SEED="${SEED:-42}"; RANDOM=$SEED
BASELINE_WAIT="${BASELINE_WAIT:-12}"
OBSERVE_AFTER="${OBSERVE_AFTER:-6}"
SETTLE="${SETTLE:-3}"
TPORT=7000; CPORT=7001; HTTP=8080
BIN_HOST="target/release/mesh"
BIN_VM="target/x86_64-unknown-linux-musl/release/mesh"
export OS_CLIENT_CONFIG_FILE="${OS_CLIENT_CONFIG_FILE:-/etc/kolla/clouds.yaml}" OS_CLOUD="${OS_CLOUD:-kolla-admin}"

[ -x "$BIN_HOST" ] || { echo "build host binary: cargo build --release -p mesh"; exit 1; }
[ -x "$BIN_VM" ]   || { echo "build static binary: cargo build --release -p mesh --target x86_64-unknown-linux-musl"; exit 1; }
[ -f "$SCENARIO" ] || { echo "no scenario: $SCENARIO"; exit 1; }
command -v openstack >/dev/null || { echo "openstack CLI required (source the kolla venv)"; exit 1; }

NETID="$(openstack network show "$NET" -f value -c id)"; NS="qdhcp-$NETID"
sudo ip netns list 2>/dev/null | grep -q "$NETID" || { echo "neutron namespace $NS missing"; exit 1; }
nse(){ sudo ip netns exec "$NS" "$@"; }
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=12 -o ServerAliveInterval=5"
vmssh(){ local ip="$1"; shift; nse ssh $SSHO cirros@"$ip" "$@" 2>/dev/null; }
vmput(){ nse ssh $SSHO cirros@"$1" "cat > $3; chmod +x $3" < "$2" 2>/dev/null; }
vmip(){ openstack server show "$1" -f value -c addresses 2>/dev/null | sed -E "s/.*'([0-9.]+)'.*/\1/"; }
status_field(){ # $1=ip $2=field
  nse curl -s --max-time 5 "http://$1:$HTTP/status" 2>/dev/null \
    | grep -oE "\"$2\":[^,}]+" | head -1 | sed -E "s/\"$2\"://; s/\"//g"; }

# device id -> VM name (cloud -> cloud-vm, edge-1 -> edge1-vm)
dev_vm(){ case "$1" in cloud) echo cloud-vm;; edge-*) echo "edge${1#edge-}-vm";; *) echo "$1-vm";; esac; }

# --- Resolve VM IPs -----------------------------------------------------------
declare -A VMIP
for vm in cloud-vm edge1-vm edge2-vm; do
  ip="$(vmip "$vm")"; [ -n "$ip" ] || { echo "VM $vm has no IP (is it ACTIVE?)"; exit 1; }
  VMIP[$vm]="$ip"
done
CLOUD_IP="${VMIP[cloud-vm]}"
echo "==> cloud-vm=${VMIP[cloud-vm]} edge1-vm=${VMIP[edge1-vm]} edge2-vm=${VMIP[edge2-vm]}" >&2

# --- Build the service/port map from the scenario -----------------------------
# Per VM, services are assigned control ports 9000,9001,... in listing order.
declare -A SVC_VM SVC_PORT DEV_OF PSEC_OF SIDE_OF; declare -A VM_NEXT
SERVICES=(); DEVICES=()
while IFS=, read -r svc dev psec side; do
  [ "$svc" = "service" ] && continue
  vm="$(dev_vm "$dev")"; idx="${VM_NEXT[$vm]:-0}"; VM_NEXT[$vm]=$((idx+1))
  SERVICES+=("$svc"); SVC_VM[$svc]="$vm"; SVC_PORT[$svc]=$((9000+idx))
  DEV_OF[$svc]="$dev"; PSEC_OF[$svc]="${psec%.*}"; SIDE_OF[$svc]="$side"
  case " ${DEVICES[*]} " in *" $dev "*) :;; *) DEVICES+=("$dev");; esac
done < <("$BIN_HOST" describe --scenario "$SCENARIO")
N_SVC=${#SERVICES[@]}

# inject target lists (host:port)
INJ_SVCS=""; for s in "${SERVICES[@]}"; do INJ_SVCS+="${VMIP[${SVC_VM[$s]}]}:${SVC_PORT[$s]},"; done; INJ_SVCS="${INJ_SVCS%,}"
INJ_COLS="${VMIP[cloud-vm]}:$CPORT,${VMIP[edge1-vm]}:$CPORT,${VMIP[edge2-vm]}:$CPORT"
inject(){ nse "$BIN_HOST" inject --scenario "$SCENARIO" --culprit "$1" --services "$INJ_SVCS" --collectors "$INJ_COLS" >/dev/null 2>&1; }

# --- Deploy the mesh on the VMs ----------------------------------------------
echo "==> deploying mesh on the VMs ($N_SVC services across ${#DEVICES[@]} devices + cloud)" >&2
for vm in cloud-vm edge1-vm edge2-vm; do
  ip="${VMIP[$vm]}"
  vmput "$ip" "$BIN_VM" /tmp/mesh || { echo "scp mesh -> $vm failed"; exit 1; }
  vmput "$ip" "$SCENARIO" /tmp/scn.yaml || { echo "scp scenario -> $vm failed"; exit 1; }
  vmssh "$ip" 'pkill -f /tmp/mesh 2>/dev/null; sleep 1; true'
  # collector name = the device this VM hosts (cloud, edge-1, edge-2)
  case "$vm" in cloud-vm) cname=cloud;; *) cname="edge-${vm#edge}"; cname="${cname%-vm}";; esac
  vmssh "$ip" "nohup /tmp/mesh collector --name $cname --port $TPORT --control-port $CPORT --http-port $HTTP --scenario /tmp/scn.yaml --stale-ms 1500 >/tmp/col.log 2>&1 &"
done
sleep 2
# start services on their VMs
for s in "${SERVICES[@]}"; do
  vm="${SVC_VM[$s]}"; ip="${VMIP[$vm]}"; cp="${SVC_PORT[$s]}"
  if [ "${SIDE_OF[$s]}" = "edge" ]; then
    # recycle 0 (persistent cloud socket): the cut here is a blackhole route that
    # severs the established connection directly, so no recycle is needed, and a
    # persistent socket keeps the cloud send from ever stalling the loop ahead of
    # the (loopback, same-VM) edge send, which would jitter edge telemetry.
    vmssh "$ip" "nohup /tmp/mesh service --name $s --culprit none --scenario /tmp/scn.yaml --edge 127.0.0.1:$TPORT --cloud $CLOUD_IP:$TPORT --control-port $cp --conn-recycle-ms 0 >/tmp/$cp.log 2>&1 &"
  else
    vmssh "$ip" "nohup /tmp/mesh service --name $s --culprit none --scenario /tmp/scn.yaml --cloud 127.0.0.1:$TPORT --control-port $cp >/tmp/$cp.log 2>&1 &"
  fi
done
echo "==> waiting ${BASELINE_WAIT}s for baselines" >&2
sleep "$BASELINE_WAIT"

# --- Self-test ----------------------------------------------------------------
cv="$(status_field "$CLOUD_IP" visible)"
echo "==> self-test: cloud sees ${cv:-?}/$N_SVC" >&2
[ -n "$cv" ] && [ "$cv" -ge $((N_SVC/2)) ] 2>/dev/null || { echo "ABORT: cloud /status not healthy (got '${cv:-empty}'); no CSV written" >&2; exit 1; }

OUT="results/production/${PLATFORM}.csv"; mkdir -p results/production
echo "platform,event,culprit,culprit_side,device,partition_sec,edge_localized,edge_ttd_ms,cloud_localized_during_cut,cloud_min_visible,n_services" > "$OUT"

# --- Event loop ---------------------------------------------------------------
echo "==> running $EVENTS events on $PLATFORM" >&2
for ev in $(seq 1 "$EVENTS"); do
  culprit="${SERVICES[$((RANDOM % N_SVC))]}"
  side="${SIDE_OF[$culprit]}"; dev="${DEV_OF[$culprit]}"; psec="${PSEC_OF[$culprit]}"
  [ "$psec" -ge 1 ] 2>/dev/null || psec=5
  echo "  event $ev/$EVENTS culprit=$culprit ($side, device=$dev, cut=${psec}s)" >&2
  edge_ttd=""; cloud_minvis=""; cloud_loc=0
  inject "$culprit"
  if [ "$side" = "edge" ]; then
    evm="$(dev_vm "$dev")"; eip="${VMIP[$evm]}"
    vmssh "$eip" "sudo ip route replace blackhole $CLOUD_IP/32" >/dev/null 2>&1
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt "$psec" ]; do
      lm="$(status_field "$eip" localized_ms)"; [ -n "$lm" ] && [ "$lm" != "null" ] && [ -z "$edge_ttd" ] && edge_ttd="$lm"
      cvv="$(status_field "$CLOUD_IP" visible)"
      [ -n "$cvv" ] && { [ -z "$cloud_minvis" ] || [ "$cvv" -lt "$cloud_minvis" ]; } && cloud_minvis="$cvv"
      cl="$(status_field "$CLOUD_IP" localized_ms)"; [ -n "$cl" ] && [ "$cl" != "null" ] && cloud_loc=1
      sleep 1
    done
    vmssh "$eip" "sudo ip route del blackhole $CLOUD_IP/32" >/dev/null 2>&1
    sleep "$OBSERVE_AFTER"
    [ -z "$edge_ttd" ] && { lm="$(status_field "$eip" localized_ms)"; [ -n "$lm" ] && [ "$lm" != "null" ] && edge_ttd="$lm"; }
  else
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt "$psec" ]; do
      cl="$(status_field "$CLOUD_IP" localized_ms)"; [ -n "$cl" ] && [ "$cl" != "null" ] && { cloud_loc=1; [ -z "$edge_ttd" ] && edge_ttd="$cl"; }
      cvv="$(status_field "$CLOUD_IP" visible)"; [ -n "$cvv" ] && { [ -z "$cloud_minvis" ] || [ "$cvv" -lt "$cloud_minvis" ]; } && cloud_minvis="$cvv"
      sleep 1
    done
    sleep "$OBSERVE_AFTER"
  fi
  edge_loc=$([ -n "$edge_ttd" ] && echo 1 || echo 0)
  echo "${PLATFORM},${ev},${culprit},${side},${dev},${psec},${edge_loc},${edge_ttd:-NA},${cloud_loc},${cloud_minvis:-NA},${N_SVC}" >> "$OUT"
  inject none
  sleep "$SETTLE"
done
echo "Done. $EVENTS events in $OUT" >&2
echo "Compare: scripts/e6/compare_prod.sh $OUT" >&2
