# SovLoc on OpenStack (two VMs: edge + cloud)

This tier runs the same edge/cloud service split on an OpenStack project you can
launch instances in (CoCIS OpenStack). The cloud half runs on a **cloud VM**,
genuinely remote from the **edge VM**, and the partition cuts the link between
them. During the cut the cloud collector still sees its three local cloud services
(3/7), so "cloud sees 0/7" is a measured outcome, not a topology guarantee.

```
edge VM (4 edge services + edge collector, runs the harness)
     |  private network (10.0.0.0/24)
     |  <-- partition: route black-hole, or security-group / iptables cut -->
cloud VM (cloud collector + enrich, store, alerting)   <-- CLOUD_URL = http://<cloud-ip>:8080
```

## 1. Launch two instances

- Image: Ubuntu 22.04/24.04. Flavor: 1 vCPU / 2 GB is plenty.
- Same project and private network; note each VM's private IP (e.g. edge
  `10.0.0.11`, cloud `10.0.0.12`). Give the edge VM a floating IP so you can SSH in.
- Install Docker on both: `sudo apt-get update && sudo apt-get install -y docker.io && sudo usermod -aG docker $USER` (re-login).
- On the edge VM, also install Rust (or copy the `mesh` binary) and clone the repo,
  because the edge half and the harness run there.

## 2. Build and push the image (once, from your workstation)

```bash
DOCKER_USER=<dockerhub-user> ./scripts/cranecloud/build_and_push.sh v2
```

## 3. Cloud VM: run the cloud half

```bash
# on the cloud VM
IMAGE=<dockerhub-user>/sovloc-mesh:v2 ./deploy/openstack/run_cloud_half.sh
curl -s http://127.0.0.1:8080/healthz    # expect: ok
```

Open **8080/tcp from the edge VM** in the cloud VM's security group:

```bash
openstack security group rule create --proto tcp --dst-port 8080 \
  --remote-ip 10.0.0.0/24 <cloud-vm-security-group>
```

## 4. Edge VM: calibrate, then run >= 30 partition events

```bash
# on the edge VM, in the repo
cargo build --release -p mesh
CLOUD=http://10.0.0.12:8080

PLATFORM=openstack CLOUD_URL=$CLOUD scripts/e6/calibrate.sh

# default cut = real route black-hole to the cloud VM (network-level, self-healing)
PLATFORM=openstack CLOUD_URL=$CLOUD EVENTS=30 scripts/e6/run_events.sh
```

To instead cut at the cloud VM's firewall (keeps edge routing intact), use:

```bash
CLOUD_SSH=ubuntu@10.0.0.12 EDGE_IP=10.0.0.11 CLOUD_PORT=8080 \
PLATFORM=openstack CLOUD_URL=$CLOUD EVENTS=30 \
  CUT_CMD='scripts/e6/cut_openstack.sh __PSEC__' scripts/e6/run_events.sh
```

## 5. Compare against the simulation

```bash
scripts/e6/compare_prod.sh results/production/openstack.csv
```

Reports edge time-to-diagnosis (median, P95) and cloud-blind rate with Wilson
intervals, and the +/-20% sim-vs-production check on the calibrated scale. Commit
`results/production/openstack.csv` and `results/production/openstack_calib.txt`.
