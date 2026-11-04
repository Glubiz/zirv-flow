#!/usr/bin/env bash
# The ingest arm: track load at the M8 rate (run_bench adds the per-second wait-event sampler).
# loadgen's report carries the per-second latency timeline (timeline_from_schedule).
# A track run also measures inbox-to-applied latency (ingest-applied.json) once the inbox has drained,
# so one run covers "Ingestion API latency" and "Inbox to applied profile" (section 19).
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"; pool="$2"
host_pg_setup
loadgen="${LOADGEN:-$root/target/release/loadgen}"
[ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
# The wait-event sampler runs for every arm: run_bench starts and stops it (instrument/waits.ndjson).
if [ -n "${M8_BATCH_SIZE:-}" ]; then
  # #76 tier-M: batch requests of M8_BATCH_SIZE items; --rate counts items per second.
  (exec "$loadgen" --url http://127.0.0.1:8080 --key-file "$M4_STATE/loadgen.key" --mode batch --batch-size "$M8_BATCH_SIZE" \
    --rate "$M8_RATE" --duration "$M8_SECONDS" --people "$pool" --out "$raw/ingest.json")
else
  since="$(db_now)"
  (track_load "$M8_SECONDS" "$pool" "$raw/ingest.json")
  measure_applied "$raw" "$since" "$raw/ingest.json" || die "inbox-to-applied latency not measured; see $raw/ingest-applied.json"
fi
