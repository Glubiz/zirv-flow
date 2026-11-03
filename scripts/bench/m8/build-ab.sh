#!/usr/bin/env bash
# Build the two binaries of the #75 decision A/B (decisions.sh) from one source tree, in a rust:1.98-bookworm container.
# Usage: build-ab.sh [git-ref]   (default origin/main). Output: $M8_STATE/ab-bin/zirv-flow-{on,off}
# CARGO_BUILD_JOBS=4 and a 4-CPU container follow the bench CPU cap. The web UI is not embedded (not used by the A/B).
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
ref="${1:-origin/main}"
M8_STATE="${M8_STATE:-$root/.zirv/work/m8}"
src="$M8_STATE/ab-src"; out="$M8_STATE/ab-bin"
win() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
rm -rf "$src"; mkdir -p "$src" "$out"
git -C "$root" archive "$ref" | tar -x -C "$src"
log "building $ref ($(git -C "$root" rev-parse --short "$ref")) as on and off"
MSYS_NO_PATHCONV=1 docker run --rm --cpus 4 -e CARGO_BUILD_JOBS=4 -v "$(win "$src"):/src:ro" -v "$(win "$out"):/out" \
  -v zirv-flow-m8-ab-target:/target -w /src rust:1.98-bookworm bash -ec '
  export CARGO_TARGET_DIR=/target
  cargo build --release --locked -p zirv-flow
  cp /target/release/zirv-flow /out/zirv-flow-on
  ZIRV_FLOW_BENCH_BUILD=1 cargo build --release --locked -p zirv-flow --features zirv-flow-store/bench-no-decisions
  cp /target/release/zirv-flow /out/zirv-flow-off'
ls -l "$out"
