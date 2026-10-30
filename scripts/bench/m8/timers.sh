#!/usr/bin/env bash
# M5's waiting-enrollment measurement on the M8 seed, once idle and once with ingest.
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"; pool="$2"
host_pg_setup
seconds="${M8_TIMERS_SECONDS:-2400}"
lead="${M8_TIMERS_LEAD_SECONDS:-600}"
drain="${M8_DRAIN_SECONDS:-3600}"
if [ -n "${M8_SMOKE:-}" ]; then seconds=5; lead=10; drain=60; fi
for name in seconds lead drain; do positive_integer "$name" "${!name}"; done
[ -x "${LOADGEN:-$root/target/release/loadgen}" ] || die 'no prebuilt loadgen'
require_job_sampler
sampler_pid=''; load_pid=''; worker_stopped=''
cleanup() {
  local pid
  for pid in "$load_pid" "$sampler_pid"; do
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  if [ -n "$worker_stopped" ]; then compose start zirv-flow-worker; fi
}
trap cleanup EXIT

for mode in noload loaded; do
  dest="$raw/$mode"
  mkdir -p "$dest"
  wait_inbox "$drain"
  worker_stopped=1
  compose stop -t 60 zirv-flow-worker
  host_pg -v seconds="$seconds" -v lead="$lead" -v enrollments="$M8_ENROLLMENTS" \
    -f "$m8_dir/timers-rearm.sql"
  compose start zirv-flow-worker
  worker_stopped=''
  wait_worker_metrics
  host_pg -At -c 'SELECT row_to_json(r) FROM m5bench.runs r WHERE run = 1' >"$dest/window.json.tmp"
  mv "$dest/window.json.tmp" "$dest/window.json"
  start_job_sampler "$dest"
  until [ "$(host_pg -At -c 'SELECT clock_timestamp() >= t0 FROM m5bench.runs WHERE run = 1')" = t ]; do sleep 1; done
  if [ "$mode" = loaded ]; then
    track_load "$seconds" "$pool" "$dest/ingest.json" >"$dest/ingest.log" 2>&1 &
    load_pid=$!
  fi
  deadline=$((SECONDS + seconds + drain))
  while :; do
    left="$(host_pg -At -c 'SELECT count(*) FROM jobs j JOIN m5bench.rows r ON r.job_id = j.id WHERE r.run = 1')"
    [ "$left" != 0 ] || break
    if [ "$SECONDS" -ge "$deadline" ]; then log "timer drain timed out with $left wake jobs; collecting $dest"; break; fi
    sleep 5
  done
  if [ -n "$load_pid" ]; then wait "$load_pid"; load_pid=''; fi
  wait_inbox "$drain"
  stop_job_sampler "$dest"
  host_pg -At -v run=1 -f "$m8_dir/../m5/lateness.sql" >"$dest/lateness.json.tmp"
  mv "$dest/lateness.json.tmp" "$dest/lateness.json"
  scrape_metrics http://127.0.0.1:9100/metrics zirv_flow_timer_wake_lateness_seconds "$dest/metrics"
  python3 -I - "$dest" "$mode" "$M8_RATE" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
late = json.loads((path / 'lateness.json').read_text())
report = dict(mode=sys.argv[2], ingest_rate=int(sys.argv[3]) if sys.argv[2] == 'loaded' else 0,
              lateness=late, distribution='uniform per workspace; no M5 burst set')
(path / 'timers.json').write_text(json.dumps(report, indent=2) + '\n')
if late['jobs_left'] or late['dead_wake_jobs'] or late['overall']['woken'] != late['overall']['seeded']:
    raise SystemExit('timer run did not wake every seeded enrollment')
PY
done
