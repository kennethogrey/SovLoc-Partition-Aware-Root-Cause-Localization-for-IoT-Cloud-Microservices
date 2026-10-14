# One-command reproduction for SovLoc.
#
# Native pipeline (needs only the pinned Rust toolchain; no TeX, no Python):
#   make reproduce     build, run every experiment, and analyse
#   make sim           run the experiment driver (writes results/raw_*.csv)
#   make analysis      compute tables, survival stats, numbers.tex, ECDF, the SVG figure
#   make env           record the build/run environment to results/environment.txt
#   make test          run the unit tests
#   make clean         remove build artifacts
#
# Container demo (needs Docker with Compose v2):
#   make demo / demo-live / inject SERVICE=x / clear / partition / netem
#   make reproduce-docker   the fully containerised reproduction
.PHONY: all reproduce sim analysis env test clean \
        reproduce-docker demo demo-live inject clear partition netem docker-clean

all: reproduce

reproduce: sim analysis
	@echo ""
	@echo "Reproduction complete. Every number and the figure come from these files:"
	@echo "  results/raw_*.csv                          raw per-run data (one row per run)"
	@echo "  results/analysis/summary_em.csv            multi-edge headline (per placement)"
	@echo "  results/analysis/summary_em_devices.csv    multi-edge per edge device"
	@echo "  results/analysis/fig_sovloc_multiedge.svg  the multi-edge results figure"
	@echo "  results/analysis/summary_e1..e7.csv        single-edge mechanism + ablations"
	@echo "  results/analysis/ecdf_e1.csv               per-cell ECDF (supplementary)"
	@echo "  results/analysis/stats.txt                 survival statistics (KM, log-rank, TOST, Holm)"
	@echo "  results/analysis/numbers.tex               LaTeX macros the manuscript cites"
	@echo "  results/analysis/fig_sovloc_results.svg    the single-edge mechanism figure"

sim:
	cargo build --release --bin sim
	./target/release/sim --out-dir results

analysis:
	cargo build --release --bin analysis
	./target/release/analysis results

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
