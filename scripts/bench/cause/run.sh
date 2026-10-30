#!/usr/bin/env bash
# Capped A/B cause experiments for #76 (batch p99) and #207 (timer lateness under ingest).
# Capped cause analysis (4 CPU / 8 GiB, ruling d84373ae): relative evidence only,
# not a section 19 measurement. One compose project, torn down with `down -v` before every run.
#
# Usage: bash scripts/bench/cause/run.sh ingest ARM RUN [RATE] [SECONDS]   ARM: A B C D (or P pilot = A)
#        bash scripts/bench/cause/run.sh timers ARM RUN                     ARM: E F G
#        bash scripts/bench/cause/run.sh down
# SQL runs through a psql shim (.zirv/m8shim/psql) that execs psql in the bench PostgreSQL container.
set -euo pipefail
cause_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
worktree="$(cd "$cause_dir/../../.." && pwd)"
export M8_PROJECT=zirv-flow-m8-cause M8_STATE="$worktree/.zirv/work/m8-cause"
export M8_PG_CPUSET=11-12 M8_SERVE_CPUSET=13 M8_WORKER_CPUSET=14 M8_PG_MEMORY=4g M8_PG_SHM=1g
export M8_SHARED_BUFFERS=1GB M8_SERVE_MEMORY=1500m M8_WORKER_MEMORY=1500m M8_SCHEDULER_MEMORY=512m
export M6_SINK_CPUSET=14 BENCH_IMAGE_TAG=0.193.0
export PATH="$worktree/.zirv/m8shim:$PATH"
# shellcheck source=../m8/run.sh
. "$cause_dir/../m8/run.sh"
configure
host_pg_setup
export BENCH_DRAIN_SHARDS=8 BENCH_WORKER_CONCURRENCY=4
label='capped cause analysis (4 CPU / 8 GiB, ruling d84373ae); relative evidence only, not a section 19 measurement'
loadgen="$root/target/release/loadgen"
[ -x "$loadgen" ] || die 'build loadgen first: cargo build --release -j 4 -p zirv-flow-loadgen --bin loadgen'
[ "$M4_PROJECT" = zirv-flow-m8-cause ] || die "refusing to touch project $M4_PROJECT"

containers=("$M4_PROJECT-postgres-1" "$M4_PROJECT-zirv-flow-1" "$M4_PROJECT-zirv-flow-worker-1" "$M4_PROJECT-zirv-flow-scheduler-1")

# Stop rules for disk and VM memory: trip tears down only this project.
monitor_loop() {
  local dir="$1" now free avail
  : >"$dir/monitor.csv"; : >"$dir/inbox.csv"
  trip() {
    echo "$1" >"$dir/STOP"
    [ ! -f "$dir/loadgen.pid" ] || kill "$(cat "$dir/loadgen.pid")" 2>/dev/null || true
    compose down -v --remove-orphans >/dev/null 2>&1 || true
    exit 3
  }
  while :; do
    now="$(date -u +%FT%TZ)"
    docker stats --no-stream --format "$now,{{.Name}},{{.CPUPerc}},{{.MemUsage}}" "${containers[@]}" 2>/dev/null >>"$dir/monitor.csv" || true
    echo "$now,$(pg -At -F, -c "SELECT count(*), coalesce(round(extract(epoch FROM now() - min(received_at))::numeric, 1), 0) FROM ingest_inbox" 2>/dev/null || echo ,)" >>"$dir/inbox.csv"
    free="$(df -k /System/Volumes/Data | awk 'NR == 2 {print $4}')"
    [ "$free" -ge $((60 * 1024 * 1024)) ] || trip "free disk below 60 GiB ($free KiB)"
    avail="$(docker exec "$M4_PROJECT-postgres-1" awk '/MemAvailable/ {print $2}' /proc/meminfo 2>/dev/null || echo 99999999)"
    echo "$now,vm_mem_available_kib,$avail,$free" >>"$dir/vm.csv"
    [ "$avail" -ge $((4 * 1024 * 1024)) ] || trip "VM available memory below 4 GiB ($avail KiB)"
    sleep 3
  done
}

port_check() {
  local port
  for port in 55432 8080 9100 9101; do
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then die "port $port is held by another process; stopping"; fi
  done
}

# Fresh volume and the same seed for every run; the knobs are in the environment of this call.
reset() {
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$M8_STATE"; mkdir -p "$M8_STATE/auth"
  port_check
  validate_cpus
  up
  if [ "$1" = 207 ]; then
    seed people; seed events; seed enrollments
  fi
  bash "$root/scripts/bench/m4/setup.sh" >/dev/null
}

finish_json() { # DIR ISSUE ARM RUN VARIABLE RC
  capture_topology "$1/topology.ndjson" || true
  python3 -I "$cause_dir/summarize.py" "$1" "$2" "$3" "$4" "$5" "$6" "$label" "$BENCH_IMAGE_TAG" \
    "$root/docs/benchmarks/data/cause/$2/$3-run$4.json"
}

ingest_run() {
  local arm="$1" run="$2" rate="${3:-2000}" seconds="${4:-300}" mode=batch variable rc=0 dir deadline
  case "$arm" in
    A | P) variable='baseline: batch 500, drain_shards 8, drain on' ;;
    B) variable='drain_shards 1'; export BENCH_DRAIN_SHARDS=1 ;;
    C) variable='drain not running (worker stopped)' ;;
    D) variable='single-item requests (track mode) at the same item rate'; mode=track ;;
    *) die "unknown ingest arm $arm" ;;
  esac
  dir="$root/.zirv/work/m8-cause-runs/76/$arm-run$run"; rm -rf "$dir"; mkdir -p "$dir"
  reset 76
  monitor_loop "$dir" & mon=$!
  [ "$arm" != C ] || compose stop -t 30 zirv-flow-worker
  curl -fsS http://127.0.0.1:8080/metrics >"$dir/serve-before.prom"
  [ "$arm" = C ] || curl -fsS http://127.0.0.1:9100/metrics >"$dir/worker-before.prom"
  echo "$rate $seconds $mode" >"$dir/params"
  "$loadgen" --url http://127.0.0.1:8080 --key-file "$M4_STATE/loadgen.key" --mode "$mode" --batch-size 500 \
    --rate "$rate" --duration "$seconds" --people 100000 --out "$dir/loadgen.json" >"$dir/loadgen.log" 2>&1 &
  echo $! >"$dir/loadgen.pid"
  wait "$(cat "$dir/loadgen.pid")" || rc=$?
  curl -fsS http://127.0.0.1:8080/metrics >"$dir/serve-after.prom" || true
  [ "$arm" = C ] || curl -fsS http://127.0.0.1:9100/metrics >"$dir/worker-after.prom" || true
  if [ "$arm" != C ] && [ ! -f "$dir/STOP" ]; then
    deadline=$((SECONDS + 300)); drain_start=$SECONDS
    until [ "$(host_pg -At -c 'SELECT count(*) FROM ingest_inbox')" = 0 ]; do
      [ "$SECONDS" -lt "$deadline" ] || break
      sleep 2
    done
    echo "$((SECONDS - drain_start))" >"$dir/drain_after_load_s"
  fi
  sleep 4
  host_pg -At -c "SELECT json_build_object('inbox_rows', count(*), 'people', (SELECT count(*) FROM people), 'events', (SELECT count(*) FROM events))
    FROM ingest_inbox" >"$dir/final-state.json" || true
  kill "$mon" 2>/dev/null || true; wait "$mon" 2>/dev/null || true
  [ ! -f "$dir/STOP" ] || { cat "$dir/STOP" >&2; return 3; }
  finish_json "$dir" 76 "$arm" "$run" "$variable" "$rc"
  return "$rc"
}

timers_run() {
  local arm="$1" run="$2" rate="${M8_RATE_F:-1000}" modes=noload variable rc=0 dir
  case "$arm" in
    E) variable='no ingest' ;;
    F) variable="track ingest at $rate items/s"; modes=loaded ;;
    G) variable="track ingest at $rate items/s, worker.concurrency 8"; modes=loaded; export BENCH_WORKER_CONCURRENCY=8 ;;
    *) die "unknown timers arm $arm" ;;
  esac
  export M8_PEOPLE=100000 M8_EVENTS=100000 M8_ENROLLMENTS=100000 M8_RATE="$rate"
  export M8_TIMERS_SECONDS=600 M8_TIMERS_LEAD_SECONDS=120 M8_DRAIN_SECONDS=1800 M8_TIMERS_MODES="$modes"
  export M8_TIMERS_POST_REARM_SQL="$cause_dir/rearm-burst.sql"
  dir="$root/.zirv/work/m8-cause-runs/207/$arm-run$run"; rm -rf "$dir"; mkdir -p "$dir/raw"
  reset 207
  monitor_loop "$dir" & mon=$!
  bash "$m8_dir/timers.sh" "$dir/raw" $((M8_PEOPLE * 2 / 5)) >"$dir/timers.log" 2>&1 &
  echo $! >"$dir/loadgen.pid"
  wait "$(cat "$dir/loadgen.pid")" || rc=$?
  if [ "$rc" = 0 ] || [ -f "$dir/raw/$modes/window.json" ]; then
    host_pg -At -f "$cause_dir/lateness-burst.sql" >"$dir/lateness-burst.json" 2>"$dir/lateness-burst.err" || true
    curl -fsS http://127.0.0.1:9100/metrics >"$dir/worker-after.prom" || true
  fi
  kill "$mon" 2>/dev/null || true; wait "$mon" 2>/dev/null || true
  [ ! -f "$dir/STOP" ] || { cat "$dir/STOP" >&2; return 3; }
  finish_json "$dir" 207 "$arm" "$run" "$variable" "$rc"
  return "$rc"
}

case "${1:-}" in
  ingest) shift; ingest_run "$@" ;;
  timers) shift; timers_run "$@" ;;
  down) compose down -v --remove-orphans ;;
  *) die 'usage: run.sh ingest ARM RUN [RATE] [SECONDS] | timers ARM RUN | down' ;;
esac
