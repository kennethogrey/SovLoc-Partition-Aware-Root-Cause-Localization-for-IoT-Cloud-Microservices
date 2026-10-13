#!/usr/bin/env bash
# The in-container reproduction. The host entry points (./run.sh and make) build
# the reproduce image and run this script inside it, with results/ mounted back
# to the host. It regenerates every number and the figure from a clean checkout,
# in Rust only: no Python, no TeX.
set -euo pipefail

mkdir -p results

echo "==> [1/5] Building the workspace (release)"
cargo build --workspace --release

echo "==> [2/5] Running the unit tests"
cargo test --workspace

echo "==> [3/5] Recording the build and run environment"
./scripts/record_env.sh || true

echo "==> [4/5] Running the seeded experiments (E1, E2, E4, E7, FP; seed 42)"
./target/release/sim --out-dir results

echo "==> [5/5] Summary tables, survival statistics, ECDF, numbers.tex, and the figure"
./target/release/analysis results

echo ""
echo "Reproduction complete. Every number and the figure come from these files:"
echo "  results/raw_*.csv                       raw per-run data (one row per run)"
echo "  results/analysis/summary_e1..e7.csv     summary tables"
echo "  results/analysis/ecdf_e1.csv            per-cell ECDF (supplementary)"
echo "  results/analysis/stats.txt              survival statistics (KM, log-rank, TOST, Holm)"
echo "  results/analysis/numbers.tex            LaTeX macros the manuscript cites"
echo "  results/analysis/fig_sovloc_results.svg the results figure"
echo "  results/environment.txt                 the machine, kernel, and toolchain"
