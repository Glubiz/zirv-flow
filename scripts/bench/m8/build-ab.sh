#!/usr/bin/env bash
# Build the two binaries of the #75 decision A/B (decisions.sh) from one source tree, in a rust:1.98-bookworm container.
# Usage: build-ab.sh [git-ref]   (default: the newest origin/main, fetched first). Output: $M8_STATE/ab-bin/zirv-flow-{on,off}
# and source-rev (the full commit both sides were built from; decisions.sh records it).
# CARGO_BUILD_JOBS=4 and a 4-CPU container follow the bench CPU cap. The web UI is not embedded (not used by the A/B).
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
if [ "$#" = 0 ]; then
  git -C "$root" fetch -q origin main || log 'fetch failed; building the local origin/main'
  ref=origin/main
else
  ref="$1"
fi
rev="$(git -C "$root" rev-parse --verify "$ref^{commit}")" || die "unknown source ref: $ref"
M8_STATE="${M8_STATE:-$root/.zirv/work/m8}"
src="$M8_STATE/ab-src"; out="$M8_STATE/ab-bin"
win() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
rm -rf "$src"; mkdir -p "$src" "$out"; rm -f "$out/source-rev"
git -C "$root" archive "$rev" | tar -x -C "$src"
log "building $ref ($rev) as on and off"
MSYS_NO_PATHCONV=1 docker run --rm --cpus 4 -e CARGO_BUILD_JOBS=4 -v "$(win "$src"):/src:ro" -v "$(win "$out"):/out" \
  -v zirv-flow-m8-ab-target:/target -w /src rust:1.98-bookworm bash -ec '
  export CARGO_TARGET_DIR=/target
  cargo build --release --locked -p zirv-flow
  cp /target/release/zirv-flow /out/zirv-flow-on
  ZIRV_FLOW_BENCH_BUILD=1 cargo build --release --locked -p zirv-flow --features zirv-flow-store/bench-no-decisions
  cp /target/release/zirv-flow /out/zirv-flow-off'
printf '%s\n' "$rev" >"$out/source-rev"
ls -l "$out"
