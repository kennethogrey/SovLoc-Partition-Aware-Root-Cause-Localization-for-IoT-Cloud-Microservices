# One-command reproduction for SovLoc.
#
# Native pipeline (needs the pinned Rust toolchain, plus unzip for the bundled
# external benchmark; no TeX, no Python):
#   make reproduce     build, run every experiment, analyse, and score the external benchmark
#   make sim           run the experiment driver (writes results/raw_*.csv)
#   make analysis      compute tables, survival stats, numbers.tex, ECDF, the SVG figure
#   make bench         score all four rankers on the bundled RCAEval RE1-OB dataset
#   make env           record the build/run environment to results/environment.txt
#   make test          run the unit tests
#   make clean         remove build artifacts
#
# Container demo (needs Docker with Compose v2):
#   make demo / demo-live / inject SERVICE=x / clear / partition / netem
#   make reproduce-docker   the fully containerised reproduction
.PHONY: all reproduce sim analysis env test clean energy bench compare \
        results-json calib footprint-capped \
        reproduce-docker demo demo-live inject clear partition netem docker-clean

all: reproduce

# The external benchmark dataset is bundled so a fresh clone reproduces everything.
# It is a .tar.gz (tar and gzip are already present everywhere, including the slim
# Docker image), so no extra tool is needed to unpack it.
RE1OB_TGZ := data/rcaeval/RE1-OB.tar.gz

reproduce: sim analysis bench compare results-json calib
	@echo ""
	@echo "Reproduction complete. Every number and the figure come from these files:"
	@echo "  results/results.json                       single machine-readable source of headline numbers"
	@echo "  results/analysis/rcaeval_comparison.csv    PPR vs BARO/N-sigma/CIRCA (paired bootstrap, Holm)"
	@echo "  results/raw_*.csv                          raw per-run data (one row per run)"
	@echo "  results/analysis/summary_em.csv            multi-edge headline (per placement)"
	@echo "  results/analysis/summary_em_devices.csv    multi-edge per edge device"
	@echo "  results/analysis/fig_sovloc_multiedge.svg  the multi-edge results figure"
	@echo "  results/analysis/summary_e1..e7.csv        single-edge mechanism + ablations"
	@echo "  results/analysis/summary_e2.csv            graph ablation (argmax, random-walk, PPR)"
	@echo "  results/analysis/summary_e5.csv            network-realism sweep (loss, jitter, flapping, skew)"
	@echo "  results/analysis/summary_ext.csv           external RCAEval RE1-OB top-1/top-3 per ranker"
	@echo "  results/analysis/ecdf_e1.csv               per-cell ECDF (supplementary)"
	@echo "  results/analysis/stats.txt                 survival statistics (KM, log-rank, TOST, Holm)"
	@echo "  results/analysis/numbers.tex               LaTeX macros the manuscript cites"
	@echo "  results/analysis/fig_sovloc_results.svg    the single-edge mechanism figure"
	@echo ""
	@echo "Separate (resource-gated) measurement:"
	@echo "  make energy          joules per diagnosis via Intel RAPL (needs sudo)"
	@echo "  make footprint-capped  collector footprint under a 1 vCPU / 512 MB cgroup cap (needs Docker)"

sim:
	cargo build --release --bin sim
	./target/release/sim --out-dir results

analysis:
	cargo build --release --bin analysis
	./target/release/analysis results

# Joules per diagnosis over a fixed localization workload (reads Intel RAPL).
energy:
	cargo build --release --bin sim
	sudo ./target/release/sim --experiments energy
	@cat results/analysis/energy.txt 2>/dev/null || true

# Run the detector + all four rankers on an external RCA dataset. With no DIR,
# it uses the bundled RCAEval RE1-OB dataset (unzipped and laid out under target/,
# which is gitignored), so a fresh clone reproduces results/analysis/summary_ext.csv.
# Pass DIR=<root> to score a different prepared dataset instead.
bench:
	cargo build --release --bin rca_bench
ifeq ($(strip $(DIR)),)
	@echo "Preparing bundled RCAEval RE1-OB dataset..."
	@rm -rf target/rcaeval && mkdir -p target/rcaeval
	@tar xzf $(RE1OB_TGZ) -C target/rcaeval
	@bash scripts/bench/prepare_re1ob.sh target/rcaeval/RE1-OB target/rcaeval/cases
	./target/release/rca_bench --dir target/rcaeval/cases --metric $(or $(METRIC),latency)
else
	./target/release/rca_bench --dir $(DIR) --metric $(or $(METRIC),latency)
endif

# Published comparators (BARO, N-sigma, CIRCA) on the same RE1-OB cases, then the
# paired bootstrap + Holm comparison against our rankers (argmax, random-walk, PPR).
# The RCAEval package is an optional heavy dependency. If it is not importable the
# baseline step is skipped with a note and the committed rcaeval_baselines*.csv are
# reused, so `make reproduce` still completes on a machine without it. The headline
# comparison is latency-only, so every method sees the same modality SovLoc uses.
compare:
	@python3 -c "import RCAEval" 2>/dev/null && { \
	  echo "Running published comparators on RE1-OB (latency metric, same modality)..."; \
	  python3 scripts/bench/run_rcaeval_baselines.py --root target/rcaeval/RE1-OB --metric-filter latency --tag _latency; \
	  echo "Running published comparators on RE1-OB (all resource metrics, for the appendix)..."; \
	  python3 scripts/bench/run_rcaeval_baselines.py --root target/rcaeval/RE1-OB; \
	} || echo "RCAEval package not importable; reusing committed results/analysis/rcaeval_baselines*.csv"
	python3 scripts/bench/compare_rankers.py --dir results/analysis --baselines-file rcaeval_baselines_percase_latency.csv

# Single machine-readable source for every headline number. Reads numbers.tex,
# stats.txt, the summary_*.csv tables, rcaeval_comparison.csv and energy.txt, and
# tags each byte/latency figure with the configuration it was measured under so
# that scale-dependent figures do not read as contradictions. Writes results/results.json.
results-json:
	python3 scripts/analysis/build_results_json.py

# B7 constrained-hardware footprint. Runs the full multi-edge mesh with the cloud
# collector and every edge collector capped at 1 vCPU / 512 MB (cgroup v2, applied
# by Docker, no sudo needed), driven through 30 real partition events. Builds the
# image once and reuses it for all ~110 containers (IMAGE=), so Compose does not try
# to build it per service. Writes results/production/single-node-mesh-capped.csv;
# fold it into results.json with `make results-json`. The collector binary and the
# 103-service workload are identical across the four tiers, so this is the footprint
# of the pipeline under the cap, not a per-orchestrator number.
footprint-capped:
	docker build -t sovloc-mesh:local .
	SCENARIO=scenarios/five-edge-100.yaml PLATFORM=single-node-mesh \
	  CAP_COLLECTORS=1 CAP_CPUS=1.0 CAP_MEM=512m IMAGE=sovloc-mesh:local EVENTS=30 \
	  bash scripts/e6/run_events_mesh.sh

# Held-out 15/15 calibration validation of the simulation against the measured
# production events (results/production/*.csv), with bootstrap ratio intervals.
# Reads the simulated edge prediction from results/results.json, so it runs after
# results-json. Writes results/production/calib_split.csv.
calib:
	python3 scripts/e6/calib_split.py

env:
	./scripts/record_env.sh

test:
	cargo test

clean:
	cargo clean

# ---- container demo ---------------------------------------------------------
reproduce-docker:
	./run.sh
demo:
	./scripts/compose_up.sh
demo-live:
	./scripts/compose_up.sh none
inject:
	./scripts/inject.sh $(SERVICE)
clear:
	./scripts/inject.sh none
partition:
	./scripts/partition_docker.sh 40
netem:
	./scripts/netem.sh cloud add 250 8 512
docker-clean:
	-docker rmi sovloc-reproduce 2>/dev/null
	-docker compose down 2>/dev/null
