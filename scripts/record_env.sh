#!/usr/bin/env bash
# Record the exact machine, kernel, toolchain, and container digests used for a
# run, so a reader can see what hardware and software produced the numbers
# (examiner: "record kernel, hardware and network"). Writes results/environment.txt.
# Pure shell; every probe is guarded so a missing tool prints "n/a" rather than
# failing the reproduction.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
mkdir -p results
OUT="results/environment.txt"

say() { printf '%s\n' "$*" >> "$OUT"; }
probe() { # label command...
  local label="$1"; shift
  local val
  val="$("$@" 2>/dev/null | head -5)" || val=""
  [ -n "$val" ] && say "$label:" && printf '%s\n' "$val" | sed 's/^/  /' >> "$OUT" || say "$label: n/a"
}

: > "$OUT"
say "SovLoc run environment"
say "Recorded: $(date -u +%Y-%m-%dT%H:%M:%SZ) (UTC)"
say "Git commit: $(git rev-parse HEAD 2>/dev/null || echo n/a)"
say ""
say "== Operating system and kernel =="
probe "uname" uname -a
if [ -r /etc/os-release ]; then say "os-release:"; sed 's/^/  /' /etc/os-release >> "$OUT"; fi
say ""
say "== CPU and memory =="
if command -v lscpu >/dev/null 2>&1; then
  say "lscpu (model, cores):"
  lscpu 2>/dev/null | grep -E "Model name|^CPU\(s\)|Socket|Thread|Core" | sed 's/^/  /' >> "$OUT"
else
  probe "cpuinfo model" sh -c "grep -m1 'model name' /proc/cpuinfo"
fi
probe "memory total" sh -c "grep MemTotal /proc/meminfo"
say ""
say "== Toolchain =="
probe "rustc" rustc -Vv
probe "cargo" cargo -V
probe "rust-toolchain.toml" cat rust-toolchain.toml
say ""
say "== Containers (for the production tiers) =="
probe "docker" docker --version
probe "docker compose" docker compose version
probe "kubectl" kubectl version --client
# Pinned base-image digests (reported if the images are present locally; the
# authoritative pins live in the Dockerfile).
probe "rust base digest" docker inspect --format='{{index .RepoDigests 0}}' rust:1.82.0-slim
probe "debian base digest" docker inspect --format='{{index .RepoDigests 0}}' debian:bookworm-slim
say ""
say "== Network interfaces and routes =="
probe "ip addr (brief)" sh -c "ip -brief addr 2>/dev/null || ifconfig -a"
probe "default route" sh -c "ip route show default 2>/dev/null || route -n"

echo "Wrote $OUT"
