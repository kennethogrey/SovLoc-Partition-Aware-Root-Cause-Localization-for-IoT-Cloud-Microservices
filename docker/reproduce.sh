#!/usr/bin/env bash
# The in-container reproduction. The host entry points (./run.sh and make) build
# the reproduce image and run this script inside it, with results/ mounted back
# to the host. It regenerates every number and figure from a clean checkout.
set -euo pipefail

mkdir -p results

echo "==> [1/4] Building the workspace (release)"
cargo build --workspace --release

echo "==> [2/4] Running the unit tests"
cargo test --workspace

echo "==> [3/4] Running the seeded experiment (30 reps per config, seed 42)"
cargo run -p sim --release -- --reps 30 --seed 42 --out-dir results

echo "==> [4/4] Confidence intervals, effect sizes, and figures"
python3 analysis/analyze.py

echo ""
echo "Reproduction complete. The numbers come from these files:"
echo "  results/summary.csv          aggregated table"
echo "  results/analysis_summary.md  results with 95% confidence intervals and effect sizes"
echo "  results/fig_latency.png, results/fig_edge_success.png"
