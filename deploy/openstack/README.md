# SovLoc on OpenStack (Kolla-Ansible all-in-one, VM-per-edge)

The OpenStack E6 tier the examiner asked for: a genuine OpenStack cloud, not a
position "from the literature". A single-host **Kolla-Ansible** all-in-one (the
examiner names Kolla-Ansible on a CoCIS server; here it runs on the project laptop)
on which the **full** multi-edge mesh -- the same `scenarios/five-edge-100.yaml`
used by the single-node and k3s tiers -- runs across **real VMs, one VM per edge
device plus a cloud VM**. A partition is a real in-VM network cut.

```
edge-1 VM (20 svc + collector) --\
edge-2 VM (15 svc + collector) ---\
edge-3 VM (25 svc + collector) ----+--> cloud VM (cloud collector + 3 cloud svc)
edge-4 VM (18 svc + collector) ---/        ^
edge-5 VM (22 svc + collector) --/         |  reached from the host through the
   tenant network 10.0.0.0/24 (neutron)    |  neutron qdhcp namespace (no floating IPs)
   partition = `ip route add blackhole <cloud-ip>` inside the edge VM (severs that device -> cloud)
```

103 services across six VMs. The mesh is the **static musl** binary run natively on
the tiny cirros guests (`cargo build --release -p mesh --target x86_64-unknown-linux-musl`),
pushed in over SSH; the host reaches the VMs through the neutron DHCP namespace
(`sudo ip netns exec qdhcp-<net-id> ssh|curl ...`), so no floating IPs are needed.

## 1. Stand up OpenStack (once)

Kolla-Ansible 2025.1 on Ubuntu 24.04, all-in-one, trimmed to keystone/glance/
placement/nova/neutron (no cinder/heat/horizon) and **HAProxy disabled** (a single
node has no spare VIP; services bind the management interface directly). A private
dummy interface `mgmt0` (10.10.10.1/24) carries the management network so no address
on the real LAN is needed; `eno1` is the neutron external interface. Host prep that
bit us, all disabled/masked before `deploy`: system mysql, rabbitmq, epmd (:4369),
memcached, libvirt (Kolla runs its own). Verify:

```bash
source ~/kolla-venv/bin/activate
export OS_CLIENT_CONFIG_FILE=/etc/kolla/clouds.yaml OS_CLOUD=kolla-admin
openstack compute service list      # nova-* up
openstack network agent list        # neutron agents :-)
```

## 2. Experiment resources (once)

```bash
openstack flavor create --ram 512  --vcpus 2 --disk 3 sovloc.e   # edge VMs
openstack flavor create --ram 1024 --vcpus 2 --disk 3 sovloc.c   # cloud VM
openstack network create sovloc-net
openstack subnet create sovloc-subnet --network sovloc-net --subnet-range 10.0.0.0/24 --dns-nameserver 8.8.8.8
openstack keypair create sovloc-key > ~/kolla/sovloc-key.pem && chmod 600 ~/kolla/sovloc-key.pem
openstack security group create sovloc-sg
openstack security group rule create --proto tcp --dst-port 1:65535 --remote-ip 10.0.0.0/24 sovloc-sg
openstack security group rule create --proto tcp --dst-port 22 sovloc-sg
openstack security group rule create --proto icmp sovloc-sg
openstack image create cirros --disk-format qcow2 --container-format bare --public \
  --file <(curl -sL http://download.cirros-cloud.net/0.6.2/cirros-0.6.2-x86_64-disk.img)
openstack server create --flavor sovloc.c --image cirros --network sovloc-net \
  --security-group sovloc-sg --key-name sovloc-key cloud-vm
for n in 1 2 3 4 5; do
  openstack server create --flavor sovloc.e --image cirros --network sovloc-net \
    --security-group sovloc-sg --key-name sovloc-key edge$n-vm; done
```

## 3. Run >= 30 events and compare

```bash
# the VMs should be freshly booted (see note below)
SCENARIO=scenarios/five-edge-100.yaml EVENTS=30 scripts/e6/run_events_openstack.sh
scripts/e6/compare_prod.sh results/production/openstack.csv
```

The harness is scenario-driven: it derives the VM set from the scenario, deploys the
mesh (one batched SSH per VM, not one per service), injects a random culprit each
event, cuts the culprit's device from the cloud with a blackhole route for the
device's partition length, polls each collector's `/status`, heals, and writes one
row (same schema as the Docker/k8s tiers).

Two conditions matter at full scale (103 services on six laptop VMs) and are handled
by the harness; miss either and localisation is silently poisoned:

- **Clean boot.** Run on freshly booted VMs. Redeploying over a still-running mesh
  leaves a stale collector/baseline that survives even a hard kill of the processes.
  `openstack server reboot <vm>` the six VMs before a run.
- **Readiness before baseline.** The 100+ cross-VM connections take ~20-30 s to all
  come up; the harness waits until the cloud sees ~all services before the per-service
  baselines freeze, so a late-connecting service still gets a clean baseline.

## What this tier shows

The same full multi-edge result as the dedicated tiers, on a real OpenStack: the edge
collector localises its device's fault during the cut (time to diagnosis a few hundred
ms), the cloud is blind during every edge-fault cut, and the cloud loses **exactly**
the cut device's services (`cloud_min_visible` = 103 minus that device's size: 78 for
edge-3's 25, 83 for edge-1's 20, 85 for edge-4's 18, 81 for edge-5's 22, 88 for
edge-2's 15). See `scripts/e6/compare_prod.sh results/production/openstack.csv`.
