# SovLoc RUNBOOK: host the code and produce the production results

This is the operator's checklist for the two things that must be done by hand:
**host the repository** the way the examiner asks, and **run the E6 production
validation** on the four mandatory platforms. Everything else (the simulation,
the statistics, the tables, and the figure) is one command and needs no cluster.

Conventions: run every command from the repository root. Replace `<...>` with your
values. `make reproduce` must have been run once so `results/analysis/` exists
(the production comparison reads the simulation reference from there).

---

## A. Reproduce the simulation locally (no cluster)

```bash
make reproduce      # build + run E1,E2,E4,E7,FP + analyse; writes results/analysis/
make test           # cargo test
make env            # record this machine to results/environment.txt
```

Outputs: `results/raw_*.csv` (raw per run), `results/analysis/summary_e1..e7.csv`,
`ecdf_e1.csv`, `stats.txt`, `numbers.tex`, and `fig_sovloc_results.svg`. Run it
twice; the numbers are identical (seed 42).

Container path (nothing but Docker on the host): `./run.sh`.

---

## B. Host the code on GitHub (tagged, reproducible)

The rebuild lives on the `multi-edge-rebuild` branch and is pushed to origin, tagged
`v3.3-reproducible` (the release the manuscript cites). From the repo root:

```bash
cd /home/ogrey/Development/masters/sovloc
git log --oneline -5           # sanity: author kennethogrey
git push origin multi-edge-rebuild
git push origin v3.3-reproducible
```

Then confirm GitHub Actions is green (two jobs: `lint-test` and `docker-reproduce`).
The release the manuscript cites is the tag `v3.3-reproducible`.

What the host now satisfies (examiner "Reproducibility statement"): public repo with
a tagged release; one command (`make reproduce` / `./run.sh`); `Cargo.lock` + pinned
`rust-toolchain.toml` + base images pinned by `@sha256` digest in the `Dockerfile`;
raw per-run CSVs committed; `results/environment.txt` records the machine, kernel,
and toolchain.

---

## C0. Multi-edge production: the full 5-edge mesh (real per-device cut)

This is the headline production tier: the whole `scenarios/five-edge-100.yaml`
topology (5 edge devices, ~20 microservices each, a distinct partition per device,
103 services + 5 edge collectors + a cloud collector) deployed for real, where a
partition is a genuine **network cut of one device from the cloud**, not an
in-process toggle. Each event injects a random culprit, cuts that culprit's device
for its partition length, and records one row: the device's edge collector
localizes its own fault during the cut, while the cloud collector loses exactly
that device's services and cannot localize the edge fault until the link heals.
Output schema matches the EM simulation, so `scripts/e6/compare_prod.sh` compares
both.

Build the mesh first: `cargo build --release -p mesh`.

### C0.1 single-node (Docker, the whole mesh on one host)

The lightest full-mesh tier: ~109 containers on one host, cut with
`docker network disconnect`. Validate on the tiny scenario first (15 containers,
seconds to come up), then run the real one.

```bash
# smoke test (tiny), proves the harness end to end
SCENARIO=scenarios/three-edge-tiny.yaml PLATFORM=single-node-mesh-tiny EVENTS=3 \
  scripts/e6/run_events_mesh.sh

# the real multi-edge run (>= 30 events); first run builds the image (~3 min)
SCENARIO=scenarios/five-edge-100.yaml PLATFORM=single-node-mesh EVENTS=30 \
  scripts/e6/run_events_mesh.sh
scripts/e6/compare_prod.sh results/production/single-node-mesh.csv
```

Each row shows `edge_localized`, `edge_ttd_ms`, `cloud_localized_during_cut` (0 for
an edge fault during its cut), and `cloud_min_visible` (drops by exactly the cut
device's size). `KEEP_UP=1` leaves the mesh running between runs.

### C0.2 k3s / OpenStack-k8s / Crane Cloud (the full mesh on a cluster)

`mesh gen-k8s` writes a Deployment+Service per microservice, one edge collector per
device, a cloud collector, a control pod, and a per-device cut NetworkPolicy. A
real partition is `kubectl apply -f cuts/cut-<id>.yaml` (restricts that device's
egress to its own pods plus DNS, blocking the cloud); healing is `kubectl delete`.
k3s enforces NetworkPolicy by default; on another cluster confirm a policy-enforcing
CNI (Calico / Cilium).

Two cluster facts the harness depends on, both learned the hard way:

- **Pod budget.** The full mesh is ~110 pods. A kubelet defaults to `max-pods=110`,
  and system pods already use ~11, so the mesh overflows and the extra pods stay
  `Pending`. Raise the cap (k3s: add `kubelet-arg: ["max-pods=150"]` to
  `/etc/rancher/k3s/config.yaml` and `sudo systemctl restart k3s`; verify with
  `kubectl get node <n> -o jsonpath='{.status.capacity.pods}'`), use a multi-node
  pool, or run a smaller scenario and say so (disclosed, not hidden).
- **A NetworkPolicy only blocks NEW connections.** The mesh service holds one
  persistent telemetry stream, so an egress cut alone would ride the established
  socket and the cloud would never go blind. `gen-k8s` therefore starts every
  service with `--conn-recycle-ms 1200`, so the service reopens its cloud socket and
  the reopen fails during the cut. The harness injects the fault 1500 ms after it
  applies the cut (LEAD_MS), which beats the recycle window plus one 200 ms report
  interval, so every pre-cut stream is re-blocked before the fault is visible and the
  cloud is blind on every event, while the recycle stays loose enough not to inflate
  the edge diagnosis time. The Docker tier needs none of this (its `network
  disconnect` severs the socket directly), and keeps the default `--conn-recycle-ms 0`.

The harness now also **self-tests the data path before writing any rows** (it probes
`cloud` and one edge `/status` through the control pod and aborts, writing no CSV, if
they do not answer) and **tears down the workloads but keeps the namespace** (deleting
the namespace can hang for minutes when a cluster add-on's aggregated API is
unhealthy, e.g. a crash-looping metrics-server stalls `metrics.k8s.io` discovery and
the namespace garbage-collector with it).

Build the image from source (it carries the `--conn-recycle-ms` flag; no pushed tag
does) and get it into the cluster, then run:

```bash
# build from source and import into k3s's containerd (no registry pull; PULL=Never).
# add `--network host` to the build if a local OpenStack/Kolla broke the docker bridge.
docker build -t sovloc-mesh:local .
docker save sovloc-mesh:local | sudo k3s ctr images import -

# --- k3s (uses the imported local image) ---
SCENARIO=scenarios/five-edge-100.yaml NAMESPACE=sovloc PLATFORM=k3s EVENTS=30 \
  K8S_IMAGE=sovloc-mesh:local IMAGE_PULL_POLICY=Never \
  scripts/e6/run_events_k8s.sh
scripts/e6/compare_prod.sh results/production/k3s.csv
```

The OpenStack and Crane Cloud tiers have their own harnesses (`run_events_openstack.sh`
over Kolla VMs, `run_events_cranecloud.sh` over the real Internet); see their sections
below.

The harness deploys `mesh.yaml`, injects and polls `/status` through the control
pod (`kubectl exec deploy/sovloc-control -- mesh status --url ...`), and tears the
mesh down at the end (`KEEP_UP=1` keeps it). Point `KUBECTL`/kubeconfig at the
target cluster for the OpenStack and Crane tiers.

---

## C. Produce the E6 production results (>= 30 events per platform)

> Lighter alternative to C0: the original 7-service cloud-half tier. Use it when a
> platform cannot host the full ~110-pod multi-edge mesh, or as a cross-check. C0
> is the headline multi-edge production.

Model used on every remote tier: the **cloud half** (cloud collector + enrich,
store, alerting) runs on the platform, genuinely remote; the **edge half** (the four
edge services + edge collector) is driven by `scripts/e6/run_events.sh`; the cut
severs the edge->cloud link. During the cut the cloud collector still sees its three
local cloud services (3/7), so "cloud sees 0/7" is a measured outcome, not a
guarantee. Each run writes `results/production/<platform>.csv`, one row per event,
with random culprit and a cut length from {10,30,60,120}s.

Build the mesh first: `cargo build --release -p mesh`.

### C.1 single-node Rust (co-located baseline, local)

Both halves run on this host; the cut drops the cloud collector's HTTP port with
iptables (the three cloud services keep reporting over TCP, so the cloud stays at
3/7). Needs sudo for the cut.

```bash
PLATFORM=single-node scripts/e6/calibrate.sh
PLATFORM=single-node EVENTS=30 scripts/e6/run_events.sh      # uses sudo scripts/e6/cut_iptables.sh
scripts/e6/compare_prod.sh results/production/single-node.csv
```

### C.2 k3s (cloud half on the cluster)

```bash
# build + push the image once
DOCKER_USER=<dockerhub-user> ./scripts/cranecloud/build_and_push.sh v9
# deploy the cloud half
sed -i "s#IMAGE_PLACEHOLDER#<dockerhub-user>/sovloc-mesh:v9#g" deploy/k3s/sovloc.yaml
kubectl label node <cloud-node> sovloc-role=cloud        # optional pinning
kubectl apply -f deploy/k3s/sovloc.yaml
kubectl get svc cloud-collector                          # note NodePort 30080
curl -s http://<node-ip>:30080/healthz                   # expect: ok

# run the edge half + >= 30 events from the harness host
CLOUD=http://<node-ip>:30080
PLATFORM=k3s CLOUD_URL=$CLOUD scripts/e6/calibrate.sh
# capture the cloud 3/7 dip from inside the cluster (the route cut blinds a poll
# from the harness):
PLATFORM=k3s CLOUD_URL=$CLOUD EVENTS=30 \
  CLOUD_PROBE_CMD='kubectl exec deploy/cloud-collector -- wget -qO- localhost:8080/status' \
  scripts/e6/run_events.sh
scripts/e6/compare_prod.sh results/production/k3s.csv
```

### C.3 OpenStack (two VMs)

Follow `deploy/openstack/README.md`: launch an edge VM and a cloud VM, run the
cloud half on the cloud VM (`deploy/openstack/run_cloud_half.sh`), open 8080 to the
edge VM, then from the edge VM:

```bash
CLOUD=http://<cloud-private-ip>:8080
PLATFORM=openstack CLOUD_URL=$CLOUD scripts/e6/calibrate.sh
PLATFORM=openstack CLOUD_URL=$CLOUD EVENTS=30 \
  CLOUD_PROBE_CMD='ssh ubuntu@<cloud-ip> curl -s localhost:8080/status' \
  scripts/e6/run_events.sh
scripts/e6/compare_prod.sh results/production/openstack.csv
```

### C.4 CRANE Cloud (managed, real Internet)

Deploy the cloud half as a single app with the entry command left blank and the env
var `SOVLOC_ROLE=cloud-half`, port 8080 (see `scripts/cranecloud/README.md`). Then:

```bash
CLOUD=http://<app>.cranecloud.io
PLATFORM=cranecloud CLOUD_URL=$CLOUD scripts/e6/calibrate.sh
PLATFORM=cranecloud CLOUD_URL=$CLOUD EVENTS=30 scripts/e6/run_events.sh
scripts/e6/compare_prod.sh results/production/cranecloud.csv
```

CRANE has no exec path, so the route cut also blinds a poll from the harness: read
the cloud's 3/7 during the cut from the Crane app logs (Application logs in the
console) and note it; the harness captures the edge side authoritatively.

### C.5 measured footprint (bytes, CPU, RSS, P99, energy)

The cloud collector counts the real telemetry bytes it receives, times each ingest
handler, and reads its own resident memory and CPU, all exposed on `GET /status`
(`bytes_in`, `p99_handler_us`, `rss_kb`, `cpu_ms`). `run_events_mesh.sh` records a
`cross_bytes`, `cpu_ms`, `rss_kb` and `p99_handler_us` column per event with no root
or iptables, and `compare_prod.sh` prints the measured footprint block. The sim's
per-sample byte constant is itself measured from the same JSON record (a test in
`crates/sim/src/model.rs` checks it), so bytes are a measurement on both sides.

Energy is separate because reading Intel RAPL needs privilege:

```bash
make energy        # sudo; writes results/analysis/energy.txt (joules per diagnosis)
```

---

## D. Collect and commit the results

```bash
git add results/production/*.csv results/production/*_calib.txt results/environment.txt
git commit -m "E6: production validation results on <platforms>"
git push origin main
# move the release tag onto the commit whose results back the manuscript, if needed:
#   git tag -f -a v3.3-reproducible -m "..." && git push -f origin v3.3-reproducible
```

`compare_prod.sh results/production/*.csv` prints one block per platform: edge time
to diagnosis (median, P95), cloud-blind rate during edge-fault cuts (Wilson), the
cloud's minimum services seen in the cut, and the +/-20% sim-vs-production check on
the calibrated scale. Report every number; where a metric falls outside +/-20%,
scope the claim rather than hiding it (the examiner's rule).

---

## E. External dataset validation (the detector on independent data)

To answer "the simulator embodies the detector's assumptions", the SAME detector and
the SAME four rankers are scored on an external benchmark with labelled root causes. The
RCAEval RE1-OB dataset (Online Boutique, metric level; arXiv 2412.17015) is **bundled in
the repository** at `data/rcaeval/RE1-OB.tar.gz`, so a fresh clone needs no download:

```bash
make bench            # unpacks the bundled dataset, lays out 125 cases, scores every ranker
```

`make bench` (also run as part of `make reproduce`) unpacks `data/rcaeval/RE1-OB.tar.gz` (with `tar xzf`) under
`target/` (gitignored), runs `scripts/bench/prepare_re1ob.sh` to lay the 125 cases out for
`rca_bench` (it normalises the `latency-50` column to `latency` and attaches the known
Online Boutique call graph in `scripts/bench/ob_graph.csv`), and writes
`results/analysis/summary_ext.csv`. The result: PPR reaches 44.8% top-1 / 71.2% top-3
across the 125 cases, ahead of argmax (38.4%) and the random walk (36.8%), and well ahead
of the published BARO, N-sigma, and CIRCA baselines (16.8%, 10.4%, 11.2% top-1 compared on
latency, in `results/analysis/rcaeval_comparison.csv`), which is the external-benchmark
comparison quoted in Section VIII of the manuscript.

To score a different dataset, pass `DIR=<root>` (and `METRIC=<name>`) where each case
subdirectory holds a `metrics.csv` (a header row then numeric rows; a leading `time`
column is ignored; `<service>_<metric>` columns collapse to one series per service), an
optional `graph.csv` (`caller,callee[,weight]`), and the root-cause service in `label.txt`:

```bash
make bench DIR=<dataset-root> METRIC=latency
```

---

## Honest limits (documented, not fabricated)

- Cloud-side faults on the remote tiers need remote injection; the harness injects
  edge-side faults there (the symmetric cloud-fault cost is shown in the simulation,
  E1). Single-node faults any of the seven.
- Energy is measured on this x86 host via Intel RAPL (`make energy`); a non-x86
  single-board-computer meter reading would extend it and is a scoped future step. One
  uncontrolled degraded-link field trial (real WiFi, ~46% loss) is run and committed as
  `results/production_field_link_*` (the edge localizes throughout; the cloud, over the
  bad link, does not). Second-coder Cohen's kappa stays the one open limitation.
