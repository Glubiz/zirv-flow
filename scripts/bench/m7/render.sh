#!/usr/bin/env bash
# Render benchmark (M7, issue #317): a typical block email (about 20 blocks, a theme, a snippet, an
# asset image, pre-evaluated conditions) through the real render pool at pool size 1 and at the
# production default, 200 warm-up renders then 5000 measured per run, and the same render
# in-process. No database, no Docker. Release build, run as-is: the script changes no CPU
# affinity, power plan or system setting, so say how the session is constrained in
# RENDER_BENCH_CONSTRAINT (it is copied into the report).
#
# Usage: scripts/bench/m7/render.sh [OUT_DIR]     (default docs/benchmarks/data/m7)
# Env:   RENDER_BENCH_CONSTRAINT  how the process is constrained, such as a CPU pin
#        RENDER_BENCH_MACHINE     the machine, if the detected CPU name is not enough
#        RENDER_BENCH_COUNT / RENDER_BENCH_WARMUP   override the 5000 / 200
# Writes OUT_DIR/render.json (the raw report) and prints a summary line per run.

set -euo pipefail
root="$(git rev-parse --show-toplevel)"
out="${1:-$root/docs/benchmarks/data/m7}"
mkdir -p "$out"

cpu="$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).Name' 2>/dev/null | tr -d '\r' || true)"
[ -n "$cpu" ] || cpu="$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || true)"
machine="${RENDER_BENCH_MACHINE:-${cpu:-unknown} ($(uname -s))}"
commit="$(git -C "$root" rev-parse --short=8 HEAD)"
git -C "$root" diff --quiet HEAD -- . ':!docs' ':!scripts' || commit="$commit+dirty"

cargo build --release -p zirv-flow-renderbench --manifest-path "$root/Cargo.toml"
target="${CARGO_TARGET_DIR:-$root/target}"
bin="$target/release/renderbench"
[ -x "$bin" ] || bin="$bin.exe"

"$bin" \
  --warmup "${RENDER_BENCH_WARMUP:-200}" --count "${RENDER_BENCH_COUNT:-5000}" \
  --machine "$machine" --constraint "${RENDER_BENCH_CONSTRAINT:-none stated}" \
  --commit "$commit" --out "$out/render.json"

cat "$out/render.json"
