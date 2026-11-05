#!/usr/bin/env bash
# PostgreSQL settings A/B for the ingest and timers arms (#397; rulings 07180bb5, f469f70c).
#   A: the current M8 settings.  B: A plus backend_flush_after=256kB, bgwriter_lru_maxpages=1000,
#   bgwriter_delay=50ms and wal_buffers=64MB.  Both sides run track_io_timing and track_wal_io_timing.
# Usage: ab-settings.sh <ingest|timers> <runs-per-side>      (ingest: 600 s, 2 runs per side)
# Runs A1 B1 A2 B2 ... (ABAB) through run.sh with M8_PG_SETTINGS=<side> and M8_IO_DIAG=1; every run is a
# normal run.sh data file (<arm>-ab-<side><n>.json). The comparison lands in ab-settings-<arm>.json beside them.
# Both sides use one image: set BENCH_IMAGE_TAG to the same tag, at least 0.233.0. Side A is restored at the end,
# also after a failed run. Env: M8_AB_RUN_CMD (default "bash <dir>/run.sh"; the self-test stubs it).
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
arm="${1:-}"; per_side="${2:-}"
case "$arm" in ingest | timers) ;; *) die 'usage: ab-settings.sh <ingest|timers> <runs-per-side>' ;; esac
positive_integer runs-per-side "$per_side"
[ "$per_side" -le 10 ] || die 'runs-per-side must be at most 10'
smoke="${M8_SMOKE:-}"
configure
python3 -I - "$BENCH_IMAGE_TAG" <<'PY' || die "BENCH_IMAGE_TAG $BENCH_IMAGE_TAG is below 0.233.0 (set the same image, at least 0.233.0, for both sides)"
import re, sys
v = tuple(int(x) for x in re.match(r'(\d+)\.(\d+)\.(\d+)', sys.argv[1]).groups())
raise SystemExit(0 if v >= (0, 233, 0) else 1)
PY
read -ra runner <<<"${M8_AB_RUN_CMD:-bash $m8_dir/run.sh}"
tsv="$(mktemp)"
finished=''
finish() {
  local rc=$? complete=0
  trap - EXIT
  [ -z "$finished" ] || complete=1
  python3 -I "$m8_dir/ab-compare.py" "$out${smoke:+/smoke}/ab-settings-$arm.json" "$arm" "$BENCH_IMAGE_TAG" "$per_side" "$complete" "$out${smoke:+/smoke}" "$tsv" ||
    { log 'could not write the comparison JSON'; rc=1; }
  log 'restoring side A'
  "${runner[@]}" pg-settings a || { log 'could not restore side A'; rc=1; }
  rm -f "$tsv"
  exit "$rc"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for ((n = 1; n <= per_side; n++)); do
  for side in a b; do
    suffix="-ab-$side$n"
    log "A/B $arm: side $side run $n of $per_side"
    rc=0
    M8_PG_SETTINGS="$side" M8_IO_DIAG=1 M8_DATA_SUFFIX="$suffix" "${runner[@]}" "$arm" || rc=$?
    data="$arm$suffix.json"
    [ "$rc" = 0 ] || data="$arm$suffix.failed.json"
    printf '%s\t%s\t%s\t%s\n' "$side" "$n" "$rc" "$data" >>"$tsv"
    [ "$rc" = 0 ] || die "$arm run $side$n failed ($rc); stopping"
  done
done
finished=1
