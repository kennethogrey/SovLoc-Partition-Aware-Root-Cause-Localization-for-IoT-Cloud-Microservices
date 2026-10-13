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

The clean rebuild is committed on `main` but not yet pushed (origin still has the
old code). From the repo root:

```bash
cd /home/ogrey/Development/masters/sovloc
git log --oneline -5           # sanity: newest commits are the rebuild, author kennethogrey
git push origin main           # fast-forwards over the old code
git tag -a v2.0-revision -m "CTA-MSC-13 revision: honest rebuild + production harness"
git push origin v2.0-revision
```

Then confirm GitHub Actions is green (two jobs: `lint-test` and `docker-reproduce`).
The release the manuscript cites is the tag `v2.0-revision`.

What the host now satisfies (examiner "Reproducibility statement"): public repo with
a tagged release; one command (`make reproduce` / `./run.sh`); `Cargo.lock` + pinned
`rust-toolchain.toml` + base images pinned by `@sha256` digest in the `Dockerfile`;
raw per-run CSVs committed; `results/environment.txt` records the machine, kernel,
and toolchain.

---

## C. Produce the E6 production results (>= 30 events per platform)

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
DOCKER_USER=<dockerhub-user> ./scripts/cranecloud/build_and_push.sh v2
# deploy the cloud half
sed -i "s#IMAGE_PLACEHOLDER#<dockerhub-user>/sovloc-mesh:v2#g" deploy/k3s/sovloc.yaml
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

### C.5 optional: measure bytes on the wire

Add `MEASURE_BYTES=1` (needs root/iptables on the edge host) to record the telemetry
bytes crossing to the cloud per event; `compare_prod.sh` then adds the +/-20% bytes
check. Where the edge host cannot see the counter, leave it off and bytes stay NA
(disclosed, not inferred).

---

## D. Collect and commit the results

```bash
git add results/production/*.csv results/production/*_calib.txt results/environment.txt
git commit -m "E6: production validation results on <platforms>"
git push origin main
# move the release tag onto the commit whose results back the manuscript, if needed:
#   git tag -f -a v2.0-revision -m "..." && git push -f origin v2.0-revision
```

`compare_prod.sh results/production/*.csv` prints one block per platform: edge time
to diagnosis (median, P95), cloud-blind rate during edge-fault cuts (Wilson), the
cloud's minimum services seen in the cut, and the +/-20% sim-vs-production check on
the calibrated scale. Report every number; where a metric falls outside +/-20%,
scope the claim rather than hiding it (the examiner's rule).

---

## Honest limits (documented, not fabricated)

- Cloud-side faults on the remote tiers need remote injection; the harness injects
  edge-side faults there (the symmetric cloud-fault cost is shown in the simulation,
  E1). Single-node faults any of the seven.
- Second-coder Cohen's kappa (E9), Raspberry-Pi energy (E4, no power meter), and an
  uncontrolled rural/mobile link (E8) are not run here; each has a protocol and stays
  an honest limitation.
