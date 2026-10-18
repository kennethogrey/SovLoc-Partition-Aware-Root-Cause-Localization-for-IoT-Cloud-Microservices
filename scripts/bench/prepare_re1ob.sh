#!/usr/bin/env bash
# Lay RCAEval RE1-OB (Online Boutique) out for rca_bench. No Python needed.
#
# RE1-OB stores each fault case as  <root>/<service>_<fault>/<run>/data.csv , with
# the culprit in the folder name and the p50 latency column named either
# "<svc>_latency-50" or "<svc>_latency". rca_bench wants, per case directory, a
# metrics.csv, a label.txt (root-cause service) and an optional graph.csv.
#
# Usage: prepare_re1ob.sh <RE1-OB-root> <out-dir>
set -euo pipefail
root="$1"; out="$2"
graph="$(cd "$(dirname "$0")" && pwd)/ob_graph.csv"
rm -rf "$out"; mkdir -p "$out"
n=0
for datacsv in "$root"/*/*/data.csv; do
  [ -f "$datacsv" ] || continue
  run="$(basename "$(dirname "$datacsv")")"
  casename="$(basename "$(dirname "$(dirname "$datacsv")")")"
  svc="$casename"
  for f in cpu delay disk loss mem; do
    case "$casename" in *_"$f") svc="${casename%_"$f"}";; esac
  done
  cdir="$out/${casename}_${run}"; mkdir -p "$cdir"
  sed '1s/_latency-50/_latency/g' "$datacsv" > "$cdir/metrics.csv"
  printf '%s\n' "$svc" > "$cdir/label.txt"
  cp "$graph" "$cdir/graph.csv"
  n=$((n+1))
done
echo "prepared $n cases in $out"
