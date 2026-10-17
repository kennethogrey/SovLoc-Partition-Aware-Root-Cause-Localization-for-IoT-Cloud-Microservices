# SovLoc on OpenStack (Kolla-Ansible all-in-one, VM-per-edge)

This is the OpenStack E6 tier the examiner asked for: a genuine OpenStack cloud,
not a position "from the literature". It is a single-host **Kolla-Ansible**
all-in-one (the examiner names Kolla-Ansible on a CoCIS server; here it runs on the
project laptop), on which the reduced multi-edge mesh runs across **real VMs, one VM
per edge device plus a cloud VM**. A partition is a real in-VM network cut.

```
edge-1 VM  (edge-1 collector + edge-1 services)  --\
edge-2 VM  (edge-2 collector + edge-2 services)  ---> cloud VM (cloud collector + cloud services)
   |  tenant network 10.0.0.0/24 (neutron)              ^
   |  partition = `ip route add blackhole <cloud-ip>`   |  reached from the host through the
   |  inside the edge VM (severs that device -> cloud)   |  neutron qdhcp namespace (no floating IPs)
```

Why reduced: the full `five-edge-100` mesh (~110 VMs) does not fit a single-host
OpenStack on a 14 GiB laptop, so the disclosed fallback is `scenarios/openstack-2edge.yaml`
(2 edge devices, uneven sizes, distinct partitions; 11 edge + 3 cloud services). It is
still genuinely multi-edge, genuinely remote, and the cut is a real route black-hole.

## 1. Stand up OpenStack (once)

Kolla-Ansible 2025.1 on Ubuntu 24.04, all-in-one, trimmed to keystone/glance/
placement/nova/neutron (no cinder/heat/horizon) and **HAProxy disabled** (a single
node has no spare VIP; services bind the management interface directly). A private
dummy interface `mgmt0` (10.10.10.1/24) carries the management network so no address
on the real LAN is needed; `eno1` is the neutron external interface. See the project
notes / `/etc/kolla/globals.yml` for the exact settings. Host prep that bit us, all
disabled/masked before `deploy`: system mysql, rabbitmq, epmd (:4369), memcached,
libvirt (Kolla runs its own). Verify:

```bash
source ~/kolla-venv/bin/activate
export OS_CLIENT_CONFIG_FILE=/etc/kolla/clouds.yaml OS_CLOUD=kolla-admin
openstack compute service list      # nova-* up
openstack network agent list        # neutron agents :-)
```

## 2. Experiment resources (once)

```bash
openstack flavor create --ram 768 --vcpus 2 --disk 3 sovloc.med   # 2 vCPU matters (below)
openstack network create sovloc-net
openstack subnet create sovloc-subnet --network sovloc-net --subnet-range 10.0.0.0/24 --dns-nameserver 8.8.8.8
openstack keypair create sovloc-key > ~/kolla/sovloc-key.pem && chmod 600 ~/kolla/sovloc-key.pem
openstack security group create sovloc-sg
openstack security group rule create --proto tcp --dst-port 1:65535 --remote-ip 10.0.0.0/24 sovloc-sg
openstack security group rule create --proto tcp --dst-port 22 sovloc-sg
openstack security group rule create --proto icmp sovloc-sg
openstack image create cirros --disk-format qcow2 --container-format bare --public \
  --file <(curl -sL http://download.cirros-cloud.net/0.6.2/cirros-0.6.2-x86_64-disk.img)
for vm in cloud-vm edge1-vm edge2-vm; do
  openstack server create --flavor sovloc.med --image cirros --network sovloc-net \
    --security-group sovloc-sg --key-name sovloc-key "$vm"; done
```

The mesh runs as the **static musl** binary natively on the tiny cirros guests
(`cargo build --release -p mesh --target x86_64-unknown-linux-musl`), pushed in over
SSH. The host reaches the VMs through the neutron DHCP namespace
(`sudo ip netns exec qdhcp-<net-id> ssh|curl ...`), so no floating IPs are needed.

## 3. Run >= 30 events and compare

```bash
SCENARIO=scenarios/openstack-2edge.yaml EVENTS=30 scripts/e6/run_events_openstack.sh
scripts/e6/compare_prod.sh results/production/openstack.csv
```

The harness deploys the mesh on the VMs, injects a random culprit each event, cuts the
culprit's device from the cloud with a blackhole route for the device's partition
length, polls each collector's `/status`, heals, and writes one row (same schema as the
Docker/k8s tiers).

## What this tier shows, and its honest limit

- **Partition, robustly measured:** on every edge-fault cut the cloud collector is blind
  (`cloud_localized_during_cut = 0`, 100% with a Wilson interval) and loses **exactly the
  cut device's services** (`cloud_min_visible` drops by that device's size, e.g. 14 -> 8
  for edge-1's six, 14 -> 9 for edge-2's five). This is the core production signal.
- **Edge time-to-diagnosis is noise-limited here and is scoped, not hidden.** A fault's
  anomaly attenuates to its immediate call-graph ancestor, so a culprit and its neighbour
  score closely; on these shared-host VMs, vCPU scheduling jitter flips that thin margin,
  so the 3-consecutive localisation window is met only intermittently (hence `2 vCPU`
  above, which helps but does not remove it). The clean edge-TtD measurement is on the
  dedicated single-node and k3s tiers, which localise 30/30. This tier's job is the
  partition evidence on a real OpenStack; the localisation-timing claim is scoped to the
  dedicated tiers, per the examiner's "scope the claim rather than hide it" rule.
