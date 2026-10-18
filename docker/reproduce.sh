#!/usr/bin/env bash
# The in-container reproduction. The host entry points (./run.sh and make) build
# the reproduce image and run this script inside it, with results/ mounted back
# to the host. It regenerates every number and the figure from a clean checkout,
# in Rust, using tar for the bundled external dataset: no Python, no TeX.
set -euo pipefail

mkdir -p results

echo "==> [1/6] Building the workspace (release)"
cargo build --workspace --release

echo "==> [2/6] Running the unit tests"
cargo test --workspace

echo "==> [3/6] Recording the build and run environment"
./scripts/record_env.sh || true

echo "==> [4/6] Running the seeded experiments (EM multi-edge, E1, E2, E4, E7, FP; seed 42)"
./target/release/sim --out-dir results

echo "==> [5/6] Summary tables, survival statistics, ECDF, numbers.tex, and the figure"
./target/release/analysis results

echo "==> [6/6] External benchmark: the same detector and rankers on the bundled RCAEval RE1-OB"
cargo build --release --bin rca_bench
rm -rf target/rcaeval && mkdir -p target/rcaeval
tar xzf data/rcaeval/RE1-OB.tar.gz -C target/rcaeval
bash scripts/bench/prepare_re1ob.sh target/rcaeval/RE1-OB target/rcaeval/cases
./target/release/rca_bench --dir target/rcaeval/cases --metric latency

echo ""
echo "Reproduction complete. Every number and the figure come from these files:"
echo "  results/raw_*.csv                          raw per-run data (one row per run)"
echo "  results/analysis/summary_em.csv            multi-edge headline (per placement)"
echo "  results/analysis/summary_em_devices.csv    multi-edge per edge device"
echo "  results/analysis/fig_sovloc_multiedge.svg  the multi-edge results figure"
echo "  results/analysis/summary_e1..e7.csv        single-edge mechanism + ablations"
echo "  results/analysis/summary_ext.csv           external RCAEval RE1-OB top-1/top-3 per ranker"
echo "  results/analysis/ecdf_e1.csv               per-cell ECDF (supplementary)"
echo "  results/analysis/stats.txt                 survival statistics (KM, log-rank, TOST, Holm)"
echo "  results/analysis/numbers.tex               LaTeX macros the manuscript cites"
echo "  results/analysis/fig_sovloc_results.svg    the single-edge mechanism figure"
echo "  results/environment.txt                    the machine, kernel, and toolchain"
