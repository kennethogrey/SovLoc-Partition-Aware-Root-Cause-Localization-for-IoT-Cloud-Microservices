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
.PHONY: all reproduce sim analysis env test clean energy bench \
        reproduce-docker demo demo-live inject clear partition netem docker-clean

all: reproduce

# The external benchmark dataset is bundled so a fresh clone reproduces everything.
# It is a .tar.gz (tar and gzip are already present everywhere, including the slim
# Docker image), so no extra tool is needed to unpack it.
RE1OB_TGZ := data/rcaeval/RE1-OB.tar.gz

reproduce: sim analysis bench
	@echo ""
	@echo "Reproduction complete. Every number and the figure come from these files:"
	@echo "  results/raw_*.csv                          raw per-run data (one row per run)"
	@echo "  results/analysis/summary_em.csv            multi-edge headline (per placement)"
	@echo "  results/analysis/summary_em_devices.csv    multi-edge per edge device"
	@echo "  results/analysis/fig_sovloc_multiedge.svg  the multi-edge results figure"
	@echo "  results/analysis/summary_e1..e7.csv        single-edge mechanism + ablations"
	@echo "  results/analysis/summary_e2.csv            ranker ablation (argmax, BARO, random-walk, PPR)"
	@echo "  results/analysis/summary_e5.csv            network-realism sweep (loss, jitter, flapping, skew)"
	@echo "  results/analysis/summary_ext.csv           external RCAEval RE1-OB top-1/top-3 per ranker"
	@echo "  results/analysis/ecdf_e1.csv               per-cell ECDF (supplementary)"
	@echo "  results/analysis/stats.txt                 survival statistics (KM, log-rank, TOST, Holm)"
	@echo "  results/analysis/numbers.tex               LaTeX macros the manuscript cites"
	@echo "  results/analysis/fig_sovloc_results.svg    the single-edge mechanism figure"
	@echo ""
	@echo "Separate (resource-gated) measurement:"
	@echo "  make energy          joules per diagnosis via Intel RAPL (needs sudo)"

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
