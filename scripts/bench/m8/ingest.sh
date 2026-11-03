#!/usr/bin/env bash
# The ingest arm: track load at the M8 rate, with the per-second wait-event sampler beside it.
# loadgen's report carries the per-second latency timeline (timeline_from_schedule).
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"; pool="$2"
host_pg_setup
loadgen="${LOADGEN:-$root/target/release/loadgen}"
[ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
waits_pid=''
cleanup() {
  if [ -n "$waits_pid" ]; then kill "$waits_pid" 2>/dev/null || true; wait "$waits_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT
mkdir -p "$raw/instrument"
host_pg -At -f "$m8_dir/pg-waits.sql" >"$raw/instrument/waits.ndjson" 2>"$raw/instrument/waits.err" &
waits_pid=$!
if [ -n "${M8_BATCH_SIZE:-}" ]; then
  # #76 tier-M: batch requests of M8_BATCH_SIZE items; --rate counts items per second.
  (exec "$loadgen" --url http://127.0.0.1:8080 --key-file "$M4_STATE/loadgen.key" --mode batch --batch-size "$M8_BATCH_SIZE" \
    --rate "$M8_RATE" --duration "$M8_SECONDS" --people "$pool" --out "$raw/ingest.json")
else
  (track_load "$M8_SECONDS" "$pool" "$raw/ingest.json")
fi
kill -0 "$waits_pid" 2>/dev/null || die "wait-event sampler exited early; see $raw/instrument/waits.err"
