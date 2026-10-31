# SovLoc: partition aware root cause localization for IoT cloud microservices

Topic CTA-MSC-13, Root Cause Analysis of Failures in Internet of Things Cloud Microservices.
Author: Kenneth Ogire, student number 2026/HD05/22778U, Group CTA-MSC-13, Makerere University.

This repository is the open source implementation and reproducibility package for
the study. It holds one non artificial intelligence root cause method run in four
placements, a deterministic simulation that measures what each placement recovers
under a network partition and at what byte cost, a production harness that runs the
same split on real infrastructure, and a one command reproduction. Every number in
the manuscript comes from a file this repository regenerates.

## What it studies

A failure in an IoT cloud application has to be diagnosed under jitter, loss, and
partial partitions of the link between the constrained edge and the cloud. The same
method, a robust change point detector that fixes when a fault began followed by a
personalized PageRank walk over the service call graph that names the culprit, is
run in four placements that differ only in where telemetry is buffered and when it
is delivered:

1. central naive: the cloud collector analyses, and edge telemetry is lost during a cut.
2. central store and forward: the edge buffers and replays to the cloud on heal.
3. edge local: a local collector analyses at the edge and ships a compact verdict.
4. hybrid: the edge detects and ranks, and ships compact evidence for cloud review.

Time to diagnosis and bytes on the wire are measured outcomes of the transport
model and the partition length, not constants. The result is a measured trade off,
not a tautology: for an edge side fault a central collector's time to diagnosis
grows with the partition while an edge local collector stays flat, and the cost is
symmetric, because for a cloud side fault the edge local collector is the one that
waits out the cut. The hybrid is fast on both at a small fraction of the bytes.

The headline system under test is a **multi-edge topology**: five edge devices
holding about twenty microservices each but deliberately uneven (20, 15, 25, 18, 22;
100 edge services plus a three service cloud back end), and a different partition
length on every device (10, 20, 30, 40, 60 s). The five distinct partitions become
the x axis of the result: the central collector's time to diagnosis climbs with the
cut on each device while the edge local and hybrid placements stay flat, and the
hybrid crosses tens of thousands of times fewer bytes. See
`results/analysis/fig_sovloc_multiedge.svg` (the multi-edge headline) and
`results/analysis/fig_sovloc_results.svg` (the single-edge version that isolates the
same mechanism on a seven service chain). The topology is data driven
(`scenarios/five-edge-100.yaml`), so it rescales by editing a file rather than the
source.

## Requirements

Either the pinned Rust toolchain plus Python 3, or just Docker.

- Native: Rust 1.82.0 (pinned in `rust-toolchain.toml`; `rustup` installs it
  automatically) and Python 3.10 or newer, standard library only. The Rust side runs
  the simulator and the benchmark; Python assembles `results/results.json`, runs the
  external RCAEval comparison, and splits the calibration. The optional `RCAEval`
  package re-runs the published comparators from their own source; without it the
  reproduction reuses the committed `results/analysis/rcaeval_baselines*.csv`. No TeX.
  Check with `cargo --version` and `python3 --version`.
- Container only: Docker with Compose v2. `docker --version && docker compose version`.

### Running on Windows (WSL)

The reproduction targets Linux. On Windows, run it inside the Windows Subsystem for
Linux (WSL 2) rather than native PowerShell, since the scripts, the network emulation,
and the container paths assume a Linux userland. Install a distribution such as Ubuntu
with `wsl --install`, open the WSL shell, install Rust and Python 3 there, then clone
the repository and run `make reproduce` from inside WSL exactly as on Linux. The Docker
path works the same way through Docker Desktop's WSL 2 backend.

## The one command

```bash
make reproduce
```

This builds the workspace, runs the experiment families (EM multi-edge placement
comparison, E1 single-edge mechanism, E2 detector and ranker ablation, E4 telemetry
cost, E7 scale, FP false alarm sweep) with seed 42, and writes the summary tables,
the survival statistics, the LaTeX macros, and the figures. The experiment is
deterministic, so the same command always produces the same numbers. Run `make test`
for the unit tests and `make env` to record the machine to
`results/environment.txt`.

Container path (nothing but Docker on the host): `./run.sh`.

## Where the numbers land

| File | What it is |
| --- | --- |
| `results/raw_em.csv`, `raw_e1..e7.csv`, `raw_fp.csv` | every run, one row each (the raw record) |
| `results/analysis/summary_em.csv` | the multi-edge headline: per placement success (Wilson), edge/cloud Kaplan-Meier median, P95, P99, measured bytes |
| `results/analysis/summary_em_devices.csv` | the multi-edge result per edge device: size, partition length, success, median time to diagnosis, bytes |
| `results/analysis/fig_sovloc_multiedge.svg` | the multi-edge results figure: time to diagnosis against each edge's partition |
| `results/analysis/summary_e1..e7.csv` | the single-edge mechanism and ablation tables: success with Wilson intervals, Kaplan-Meier median, IQR, P95, P99, measured bytes |
| `results/analysis/ecdf_e1.csv` | the per cell empirical CDF (censoring aware), supplementary |
| `results/analysis/stats.txt` | the survival statistics: slopes with bootstrap CI, log-rank, TOST equivalence at partition zero, Holm correction |
| `results/analysis/numbers.tex` | the LaTeX macros the manuscript cites, so no number is typed by hand |
| `results/analysis/fig_sovloc_results.svg` | the results figure, generated from the run data |
| `results/environment.txt` | the machine, kernel, and toolchain for this run |

## Reproducibility and hosting

The build is pinned at three levels: `rust-toolchain.toml` fixes the compiler at
1.82.0, `Cargo.lock` is committed, and the `Dockerfile` pins both base images by
`@sha256` digest. The raw per run CSVs are committed, not only the summaries, and
`results/environment.txt` records the hardware, kernel, and toolchain. GitHub
Actions re-runs the suite and the Docker reproduction on every push. The release
that matches the manuscript is the annotated tag `v3.3-reproducible`.

## Production validation (E6)

The simulation is one tier. The production tier runs the same edge and cloud
service split on real infrastructure, so the cloud side is genuinely remote from
the edge side and a partition is a real network cut rather than an in process
toggle.

The headline production tier is the **full multi-edge mesh**: `mesh gen-compose`
(Docker) and `mesh gen-k8s` (Kubernetes) stand up the whole
`scenarios/five-edge-100.yaml` topology (103 services, 5 edge collectors, a cloud
collector), and a partition is a real per-device network cut, `docker network
disconnect` for a device's net on Docker or a per-device `NetworkPolicy` on a
cluster. `scripts/e6/run_events_mesh.sh` (Docker) and `scripts/e6/run_events_k8s.sh`
(k3s / OpenStack / Crane Cloud) run at least thirty events and record one row per
event; during each edge-device cut the edge collector keeps localizing that
device's fault while the cloud collector loses exactly that device's services.
`RUNBOOK.md` section C0 has the step by step. The lighter 7-service cloud-half tier
below remains as a cross-check for platforms that cannot host the full mesh. The cloud half (the cloud collector plus enrich, store, and alerting) runs
on the platform; the edge half (the four edge services plus the edge collector) is
driven by the harness; and the cut severs the edge to cloud link. During the cut
the cloud collector still sees its three local cloud services, so "cloud sees 0 of
7" is a measured outcome of the cut, not a guarantee of the topology.

`RUNBOOK.md` has the step by step procedure for all four mandatory platforms:
single node mesh, k3s, OpenStack, and CRANE Cloud. At least thirty independent
partition events per platform, with random culprits and per-device cut lengths from
10 to 60 seconds, are recorded one row per event and compared against the simulation
within the plus or minus twenty percent tolerance:

```bash
# the full 103-service mesh, >= 30 events per tier (k3s/OpenStack/CRANE in RUNBOOK.md):
SCENARIO=scenarios/five-edge-100.yaml PLATFORM=single-node-mesh EVENTS=30 \
  IMAGE=sovloc-mesh:local scripts/e6/run_events_mesh.sh
scripts/e6/compare_prod.sh results/production/single-node-mesh.csv
```

## What is inside

| Path | What it is |
| --- | --- |
| `crates/rca-core` | the non artificial intelligence RCA library: robust change point, robust scorer, personalized PageRank, and the survival statistics (Wilson, Kaplan-Meier, log-rank, TOST, Holm, bootstrap), unit tested |
| `crates/sim` | the deterministic seeded simulation (`sim`) and the analysis binary (`analysis`) that turns the raw CSVs into tables, statistics, `numbers.tex`, and the figure |
| `crates/mesh` | the real networked mesh: real services and two live collectors over TCP and HTTP, the production tier |
| `crates/scenario` | reads a YAML topology (many edge devices, each with its own microservice count and partition time) that drives both the multi-edge simulation headline (`scenarios/five-edge-100.yaml`) and the configurable production mesh |
| `scripts/e6/` | the E6 production harness: `run_events_mesh.sh` (full multi-edge mesh on Docker), `run_events_k8s.sh` (multi-edge on a cluster), `run_events.sh` (7-service cloud-half tier), `calibrate.sh` (E3), `compare_prod.sh`, and the per platform cut scripts |
| `scripts/cranecloud/` | the CRANE Cloud real Internet tier: build and push, deploy, probe, and partition the link |
| `deploy/k3s/`, `deploy/openstack/` | the cloud half manifests and runbooks for the cluster tiers |
| `Dockerfile`, `docker/` | the pinned reproduction and runtime images |
| `.github/workflows/ci.yml` | the pipeline that re-runs the suite and the Docker reproduction on every push |
| `results/` | the committed raw data, the analysis outputs, and the recorded environment |

## Honest limits

The simulation numbers are modelled from documented assumptions rather than
physically measured, and the internal accuracy is high because the generator's
ground truth is clean and the topologies are small. That is why the study does not
rest on the simulation alone. The same detector and ranker are scored on the
external RCAEval RE1-OB benchmark (real third-party fault data, part of `make
reproduce`), where personalized PageRank reaches 44.8% top-1, ahead of the published
BARO, N-sigma, and CIRCA baselines; and the four production tiers run the same
pipeline under real network cuts, where the calibrated simulation matches the
measured edge median within twenty percent on all four. Energy is measured on this
x86 host via Intel RAPL; a single-board-computer meter reading would extend it to
non-x86 and is a scoped future step, not a gap in the reported figure. One
uncontrolled degraded-link field trial over a real WiFi connection (about 46% loss)
is included, where the edge collector keeps localizing the fault while the cloud
collector, reached over the bad link, does not. The one limitation that stays open
is second-coder agreement (Cohen's kappa): the literature screening is a
single-reviewer classification with the coding published, and a second human coder
has not been run. The use of a generative AI assistant during preparation is
disclosed in the manuscript.

## Release

```bash
git clone https://github.com/kennethogrey/SovLoc-Partition-Aware-Root-Cause-Localization-for-IoT-Cloud-Microservices.git sovloc
cd sovloc
git checkout v3.3-reproducible
make reproduce
```
