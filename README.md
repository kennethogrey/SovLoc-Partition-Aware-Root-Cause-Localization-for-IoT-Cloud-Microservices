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
See `results/analysis/fig_sovloc_results.svg`.

## Requirements

Either the pinned Rust toolchain, or just Docker.

- Native: Rust 1.82.0 (pinned in `rust-toolchain.toml`; `rustup` installs it
  automatically). No Python, no TeX. Check with `cargo --version`.
- Container only: Docker with Compose v2. `docker --version && docker compose version`.

## The one command

```bash
make reproduce
```

This builds the workspace, runs the five experiment families (E1 placement
comparison, E2 detector and ranker ablation, E4 telemetry cost, E7 scale, FP false
alarm sweep) with seed 42, and writes the summary tables, the survival statistics,
the LaTeX macros, and the figure. The experiment is deterministic, so the same
command always produces the same numbers. Run `make test` for the unit tests and
`make env` to record the machine to `results/environment.txt`.

Container path (nothing but Docker on the host): `./run.sh`.

## Where the numbers land

| File | What it is |
| --- | --- |
| `results/raw_e1..e7.csv`, `raw_fp.csv` | every run, one row each (the raw record) |
| `results/analysis/summary_e1..e7.csv` | the summary tables: success with Wilson intervals, Kaplan-Meier median, IQR, P95, P99, measured bytes |
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
that matches the manuscript is the annotated tag `v2.0-revision`.

## Production validation (E6)

The simulation is one tier. The production tier runs the same edge and cloud
service split on real infrastructure, so the cloud side is genuinely remote from
the edge side and a partition is a real network cut rather than an in process
toggle. The cloud half (the cloud collector plus enrich, store, and alerting) runs
on the platform; the edge half (the four edge services plus the edge collector) is
driven by the harness; and the cut severs the edge to cloud link. During the cut
the cloud collector still sees its three local cloud services, so "cloud sees 0 of
7" is a measured outcome of the cut, not a guarantee of the topology.

`RUNBOOK.md` has the step by step procedure for all four mandatory platforms:
single node Rust, k3s, OpenStack (two VMs), and CRANE Cloud. At least thirty
independent partition events per platform, with random culprits and cut lengths
from 10 to 120 seconds, are recorded one row per event and compared against the
simulation within the plus or minus twenty percent tolerance:

```bash
PLATFORM=single-node scripts/e6/calibrate.sh          # E3: measure the real tick
PLATFORM=single-node EVENTS=30 scripts/e6/run_events.sh
scripts/e6/compare_prod.sh results/production/single-node.csv
```

## What is inside

| Path | What it is |
| --- | --- |
| `crates/rca-core` | the non artificial intelligence RCA library: robust change point, robust scorer, personalized PageRank, and the survival statistics (Wilson, Kaplan-Meier, log-rank, TOST, Holm, bootstrap), unit tested |
| `crates/sim` | the deterministic seeded simulation (`sim`) and the analysis binary (`analysis`) that turns the raw CSVs into tables, statistics, `numbers.tex`, and the figure |
| `crates/mesh` | the real networked mesh: real services and two live collectors over TCP and HTTP, the production tier |
| `crates/scenario` | reads a YAML topology (many edge devices, each with its own partition time) for the configurable mesh |
| `scripts/e6/` | the E6 production harness: `run_events.sh`, `calibrate.sh` (E3), `compare_prod.sh`, and the per platform cut scripts |
| `scripts/cranecloud/` | the CRANE Cloud real Internet tier: build and push, deploy, probe, and partition the link |
| `deploy/k3s/`, `deploy/openstack/` | the cloud half manifests and runbooks for the cluster tiers |
| `Dockerfile`, `docker/` | the pinned reproduction and runtime images |
| `.github/workflows/ci.yml` | the pipeline that re-runs the suite and the Docker reproduction on every push |
| `results/` | the committed raw data, the analysis outputs, and the recorded environment |

## Honest limits

The simulation numbers are modelled from documented assumptions, not physically
measured, and the simulated accuracy is high because the generator's ground truth
is clean and the topologies are small; a harder public benchmark would score lower,
and that external check is noted as remaining. The production harness and the cut
are real network validation. Second coder agreement (Cohen's kappa), Raspberry Pi
energy, and an uncontrolled rural link are not run here; each has a written protocol
and stays an honest limitation rather than an invented number. The use of a
generative AI assistant during preparation is disclosed in the manuscript.

## Release

```bash
git clone https://github.com/kennethogrey/SovLoc-Partition-Aware-Root-Cause-Localization-for-IoT-Cloud-Microservices.git sovloc
cd sovloc
git checkout v2.0-revision
make reproduce
```
