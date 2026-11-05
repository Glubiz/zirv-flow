# shellcheck shell=bash
set -euo pipefail
m8_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$m8_dir/../../.." && pwd)"
cd "$root"
label='reference-resource emulation, not a reference-hardware run'
benches=(ingest burst debugger explorer jobs timers decisions send render segments)
# Git Bash rewrites container paths (/bench/..., /usr/local/...) in docker arguments; other hosts ignore this.
export MSYS2_ARG_CONV_EXCL='/bench/;/usr/local/;/var/lib/'
log() { echo "[m8] $*" >&2; }
die() { log "$*"; exit 1; }

configure() {
  export M8_PG_CPUSET="${M8_PG_CPUSET:-8-15}" M8_SERVE_CPUSET="${M8_SERVE_CPUSET:-0-3}"
  export M8_WORKER_CPUSET="${M8_WORKER_CPUSET:-4-7}"
  export M8_PG_MEMORY="${M8_PG_MEMORY:-32g}" M8_SERVE_MEMORY="${M8_SERVE_MEMORY:-8g}"
  export M8_WORKER_MEMORY="${M8_WORKER_MEMORY:-7g}" M8_SCHEDULER_MEMORY="${M8_SCHEDULER_MEMORY:-1g}"
  export M8_PG_IMAGE="${M8_PG_IMAGE:-postgres:16.15}"
  export BENCH_IMAGE_TAG="${BENCH_IMAGE_TAG:-0.190.0}"
  [[ "$BENCH_IMAGE_TAG" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][a-zA-Z0-9.-]+)?$ ]] || die 'BENCH_IMAGE_TAG must be a version, never latest'
  export M8_STATE="${M8_STATE:-$root/.zirv/work/m8}"
  export M4_PROJECT="zirv-flow-m8${smoke:+-smoke}" M4_STATE="$M8_STATE${smoke:+-smoke}/auth"
  export M4_RESOURCE_COMPOSE="$m8_dir/compose.m8.yaml"
  export POSTGRES_USER=zirv POSTGRES_DB=zirv_flow_m8
  unset M4_EXTRA_COMPOSE
  [ -z "${M8_PG_SETTINGS:-}" ] || pg_side_env "$M8_PG_SETTINGS"
  export M8_PEOPLE="${M8_PEOPLE:-25000000}" M8_EVENTS="${M8_EVENTS:-500000000}"
  export M8_ENROLLMENTS="${M8_ENROLLMENTS:-10000000}" M8_CHUNK="${M8_CHUNK:-100000}"
  export M8_SECONDS="${M8_SECONDS:-600}" M8_RATE="${M8_RATE:-2000}"
  if [ -n "${smoke:-}" ]; then
    export M8_PEOPLE=5000 M8_EVENTS=5000 M8_ENROLLMENTS=3000 M8_SECONDS=60 M8_RATE=50
    export M4_RUNS=2 M4_REFRESH_RUNS=1 RENDER_BENCH_COUNT=20 RENDER_BENCH_WARMUP=3
  fi
  python3 -I - <<'PY'
import os
for name in ('M8_PEOPLE', 'M8_EVENTS', 'M8_ENROLLMENTS', 'M8_CHUNK', 'M8_SECONDS', 'M8_RATE'):
    value = os.environ[name]
    if not value.isdecimal() or not 0 < int(value) <= 500_000_000:
        raise SystemExit(f'{name} must be an integer in 1..500000000')
if not 5 <= int(os.environ['M8_PEOPLE']) <= 25_000_000:
    raise SystemExit('M8_PEOPLE must be in 5..25000000 (three workspaces, none above 10 M)')
if int(os.environ['M8_ENROLLMENTS']) > int(os.environ['M8_PEOPLE']):
    raise SystemExit('M8_ENROLLMENTS must not exceed M8_PEOPLE')
PY
  out="${M8_DATA_DIR:-$root/docs/benchmarks/data/m8}"
}

compose() {
  docker compose -p "$M4_PROJECT" -f docker-compose.yml -f scripts/bench/compose.bench.yaml \
    -f "$M4_RESOURCE_COMPOSE" "$@"
}
pg() { compose exec -T postgres psql -X -q -U zirv -d zirv_flow_m8 -v ON_ERROR_STOP=1 "$@"; }

# PostgreSQL settings A/B (#397). Side A is the current M8 settings; side B adds backend_flush_after,
# bgwriter_lru_maxpages, bgwriter_delay and wal_buffers. Both sides time I/O, so the diagnostic cost is equal.
# The variables are read by compose.m8.yaml, so every compose call of a run must see the same side.
pg_side_names='track_io_timing track_wal_io_timing backend_flush_after bgwriter_lru_maxpages bgwriter_delay wal_buffers'
pg_side_env() {
  export M8_PG_TRACK_IO_TIMING=on
  case "$1" in
    a) export M8_PG_BACKEND_FLUSH_AFTER=0 M8_PG_BGWRITER_MAXPAGES=100 M8_PG_BGWRITER_DELAY=200ms M8_PG_WAL_BUFFERS=-1 ;;
    b) export M8_PG_BACKEND_FLUSH_AFTER=256kB M8_PG_BGWRITER_MAXPAGES=1000 M8_PG_BGWRITER_DELAY=50ms M8_PG_WAL_BUFFERS=64MB ;;
    *) die 'M8_PG_SETTINGS must be a or b' ;;
  esac
}
# SHOW values of the A/B settings as one JSON object.
pg_side_show() {
  pg -At -c "SELECT json_object_agg(name, current_setting(name)) FROM unnest(string_to_array('${pg_side_names// /,}', ',')) name"
}
# Fails unless every setting of the side took effect. Side A's wal_buffers is the automatic size: 16MB at 8GB shared_buffers.
verify_pg_side() {
  local show
  show="$(pg_side_show)" || die 'cannot read the PostgreSQL settings'
  python3 -I - "$1" "$show" <<'PY'
import json, sys
side, show = sys.argv[1], json.loads(sys.argv[2].strip())
common = {'track_io_timing': 'on', 'track_wal_io_timing': 'on'}
expected = {'a': dict(common, backend_flush_after='0', bgwriter_lru_maxpages='100', bgwriter_delay='200ms', wal_buffers='16MB'),
            'b': dict(common, backend_flush_after='256kB', bgwriter_lru_maxpages='1000', bgwriter_delay='50ms', wal_buffers='64MB')}[side]
bad = [f'{k} is {show.get(k)!r}, expected {v!r}' for k, v in expected.items() if show.get(k) != v]
if bad:
    raise SystemExit(f'PostgreSQL settings side {side} did not take effect: ' + '; '.join(bad))
PY
}
# Recreates only the PostgreSQL container with the side's settings (wal_buffers needs a restart). No volume is
# removed, nothing is reseeded and no other container is touched; their pools reconnect.
switch_pg_settings() {
  local side="$1" deadline
  pg_side_env "$side"
  log "PostgreSQL settings side $side: recreating only the postgres container (restart; volumes kept)"
  pg -c CHECKPOINT >/dev/null 2>&1 || log 'checkpoint before the switch failed; the restart will recover from WAL'
  compose up -d --no-build --no-deps --force-recreate --timeout 600 --wait --wait-timeout 600 postgres
  deadline=$((SECONDS + 300))
  until [ "$(pg -At -c 'SELECT 1' 2>/dev/null | tr -d '\r')" = 1 ]; do
    [ "$SECONDS" -lt "$deadline" ] || die 'PostgreSQL did not accept connections after the settings switch'
    sleep 2
  done
  verify_pg_side "$side"
  wait_app_ready
}
wait_app_ready() {
  local deadline=$((SECONDS + 180))
  until curl -fsS --connect-timeout 2 --max-time 5 http://127.0.0.1:8080/metrics >/dev/null 2>&1; do
    [ "$SECONDS" -lt "$deadline" ] || die 'serve did not answer after the settings switch'
    sleep 2
  done
}

# compose.m8.yaml fixes these credentials; compose.bench.yaml publishes this host port.
host_pg_setup() {
  command -v psql >/dev/null || die 'host psql is required (same host as loadgen, with python3); install the PostgreSQL client'
  export PGHOST=127.0.0.1 PGPORT=55432 PGUSER=zirv PGDATABASE=zirv_flow_m8 PGPASSWORD=zirv-m8-bench
  export PGCONNECT_TIMEOUT=10
}
host_pg() { psql -X -q -v ON_ERROR_STOP=1 "$@"; }

positive_integer() {
  [[ "$2" =~ ^[1-9][0-9]*$ ]] && [ "$2" -le 500000000 ] || die "$1 must be an integer in 1..500000000"
}

# exec replaces the calling process, so a backgrounded call's $! is loadgen itself; run it in ( ) in the foreground.
track_load() {
  exec "${LOADGEN:-$root/target/release/loadgen}" --url http://127.0.0.1:8080 \
    --key-file "$M4_STATE/loadgen.key" --mode track --rate "$M8_RATE" --duration "$1" \
    --people "$2" --out "$3"
}

wait_inbox() {
  local deadline=$((SECONDS + $1))
  until [ "$(host_pg -At -c 'SELECT count(*) FROM ingest_inbox')" = 0 ]; do
    [ "$SECONDS" -lt "$deadline" ] || die 'inbox did not drain before the timeout'
    sleep 1
  done
}

# Keep the Prometheus samples verbatim, including buckets, in the JSON gathered by write_data.
scrape_metrics() {
  curl -fsS --connect-timeout 5 --max-time 10 "$1" >"$3.prom"
  python3 -I - "$2" "$3" <<'PY'
import json, pathlib, sys
prefixes, dest = sys.argv[1:]
lines = [line for line in pathlib.Path(dest + '.prom').read_text().splitlines()
         if line.startswith(tuple(prefixes.split(',')))]
pathlib.Path(dest + '.json').write_text(json.dumps({'samples': lines}, indent=2) + '\n')
PY
}

require_job_sampler() {
  local tool
  for tool in jq curl awk; do command -v "$tool" >/dev/null || die "$tool is required by the per-kind sampler"; done
  [ -f "$m8_dir/sample-job-kinds.sh" ] && [ -f "$m8_dir/job-kinds.sql" ] || die 'per-kind sampler is missing; merge #455 first'
}

start_job_sampler() {
  (export PGAPPNAME=m8-sampler-jobkinds; exec bash "$m8_dir/sample-job-kinds.sh") >"$1/job-kinds.csv" 2>"$1/job-kinds.err" &
  sampler_pid=$!
  register_sampler jobkinds "$sampler_pid"
}

wait_worker_metrics() {
  local deadline=$((SECONDS + 60))
  until curl -fsS --connect-timeout 2 --max-time 5 http://127.0.0.1:9100/metrics >/dev/null 2>&1; do
    [ "$SECONDS" -lt "$deadline" ] || die 'worker metrics did not become ready'
    sleep 1
  done
}

stop_job_sampler() {
  local rc=0 lines deadline
  # Get a sample after completion, even when smoke drains between two sampler ticks.
  lines="$(wc -l <"$1/job-kinds.csv")"
  deadline=$((SECONDS + $(python3 -I -c 'import math,sys; print(math.ceil(float(sys.argv[1])) + 30)' "${JOB_KINDS_TICK_S:-5}")))
  while :; do
    kill -0 "$sampler_pid" 2>/dev/null || { wait "$sampler_pid" || rc=$?; die "per-kind sampler exited early ($rc); see $1/job-kinds.err"; }
    [ "$(wc -l <"$1/job-kinds.csv")" -gt "$lines" ] && break
    [ "$SECONDS" -lt "$deadline" ] || die 'per-kind sampler did not produce a final sample'
    sleep 1
  done
  kill "$sampler_pid"
  wait "$sampler_pid" || rc=$?
  unregister_sampler jobkinds "$sampler_pid"
  sampler_pid=''
  terminate_pg_samplers m8-sampler-jobkinds
  [ "$rc" = 143 ] || [ "$rc" = 0 ] || die "per-kind sampler failed ($rc); see $1/job-kinds.err"
  python3 -I - "$1" <<'PY'
import csv, json, pathlib, sys
path = pathlib.Path(sys.argv[1])
rows = list(csv.DictReader((path / 'job-kinds.csv').open()))
if not rows:
    raise SystemExit('per-kind sampler returned no samples')
for row in rows:
    for key in row.keys() - {'t', 'kind'}:
        row[key] = float(row[key]) if row[key] else None
(path / 'job-kinds.json').write_text(json.dumps(rows, indent=2) + '\n')
PY
}

# Quiet start: an arm that follows seed or timers work waits for vacuum to finish, then checkpoints,
# so background work from that earlier work does not land in its window.
quiet_start() {
  local timeout="${M8_QUIET_TIMEOUT_S:-1800}" deadline busy
  positive_integer M8_QUIET_TIMEOUT_S "$timeout"
  deadline=$((SECONDS + timeout))
  log 'quiet start: waiting for pg_stat_progress_vacuum to empty'
  while :; do
    busy="$(pg -At -c 'SELECT count(*) FROM pg_stat_progress_vacuum')"
    [ "$busy" = 0 ] && break
    [ "$SECONDS" -lt "$deadline" ] || die "quiet start: $busy vacuum(s) still running after ${timeout}s (M8_QUIET_TIMEOUT_S); arm not started"
    sleep 5
  done
  pg -c CHECKPOINT
}

# Background samplers. Every sampler a bench starts is registered in $M8_STATE/samplers (one file per
# pid) and its database sessions carry a PGAPPNAME starting with m8-sampler, so a leftover is found both
# on the host (a live pid) and in PostgreSQL (a session). A run stops its samplers on every exit path and
# refuses to start while one from an earlier run is still alive.
sampler_dir() { echo "$M8_STATE/samplers"; }
register_sampler() { mkdir -p "$(sampler_dir)" && : >"$(sampler_dir)/$1.$2"; }
unregister_sampler() { rm -f "$(sampler_dir)/$1.$2"; }
stop_sampler() { # name pid
  [ -z "${2:-}" ] || { kill "$2" 2>/dev/null || true; wait "$2" 2>/dev/null || true; }
  [ -z "${2:-}" ] || unregister_sampler "$1" "$2"
}
# Ends the sessions of every database sampler, whatever their host process did.
terminate_pg_samplers() { # optional $1: application_name to end (default every sampler)
  pg -At -c "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name LIKE '${1:-m8-sampler%}' AND pid <> pg_backend_pid()" >/dev/null 2>&1 ||
    log 'could not end the database sampler sessions'
}
# Stops what this run registered that is still alive (a child that was killed hard leaves its sampler behind).
stop_registered_samplers() {
  local f pid
  for f in "$(sampler_dir)"/*.*; do
    [ -e "$f" ] || continue
    pid="${f##*.}"
    kill "$pid" 2>/dev/null || true
    rm -f "$f"
  done
  terminate_pg_samplers
}
# Refuses to start while a sampler of an earlier run is alive; clears registrations of dead pids.
assert_no_samplers() {
  local f pid live=() sessions
  for f in "$(sampler_dir)"/*.*; do
    [ -e "$f" ] || continue
    pid="${f##*.}"
    if kill -0 "$pid" 2>/dev/null; then live+=("${f##*/}"); else rm -f "$f"; fi
  done
  [ "${#live[@]}" = 0 ] || die "a sampler of an earlier run is still alive (${live[*]}); stop those processes and remove their files in $(sampler_dir)"
  sessions="$(pg -At -c "SELECT count(*) FROM pg_stat_activity WHERE application_name LIKE 'm8-sampler%'")"
  [ "$sessions" = 0 ] || die "$sessions database sampler session(s) from an earlier run are alive; end them with: SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name LIKE 'm8-sampler%'"
}

# The wait-event sampler (pg-waits.sql, once per second) of an arm.
waits_pid=''
start_waits_sampler() {
  local keyed=()
  # M8_IO_DIAG: the same sampler at 10 Hz, keyed by application_name and backend_type (no second sampler).
  [ -z "${M8_IO_DIAG:-}" ] || keyed=(-v interval="${M8_WAITS_INTERVAL_S:-0.1}" -v keyed=1)
  (export PGAPPNAME=m8-sampler-waits; exec psql -X -q -v ON_ERROR_STOP=1 -At "${keyed[@]}" -f "$m8_dir/pg-waits.sql") >"$1/waits.ndjson" 2>"$1/waits.err" &
  waits_pid=$!
  register_sampler waits "$waits_pid"
}
stop_waits_sampler() {
  stop_sampler waits "$waits_pid"
  waits_pid=''
  terminate_pg_samplers
}

# Per-arm instrumentation, shared by every arm (called around the arm's command by run_bench).
# Everything lands in $1/instrument: serve metrics, PostgreSQL WAL/I/O/checkpoint snapshots,
# the top statements by execution time, the wait-event sampler and docker stats of the stack.
# docker stats: one JSON line per container per sample, keyed by container name (a name survives
# the recreate of a service; an ID does not), every M8_DOCKER_STATS_TICK_S (1) seconds after the previous sample.
stats_pid=''
stop_docker_stats() {
  stop_sampler docker-stats "$stats_pid"
  stats_pid=''
}
stack_container_names() {
  local service
  for service in postgres zirv-flow zirv-flow-worker zirv-flow-scheduler; do echo "${M4_PROJECT}-${service}-1"; done
}
docker_stats_sample() { # one tick for the container names in "$@"; a missing container (mid-recreate) costs only its own line
  local now format
  now="$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)"
  format='{"t":"'"$now"'","name":"{{.Name}}","cpu":"{{.CPUPerc}}","mem":"{{.MemUsage}}","mem_pct":"{{.MemPerc}}","net":"{{.NetIO}}","block":"{{.BlockIO}}","pids":"{{.PIDs}}"}'
  docker stats --no-stream --format "$format" "$@" 2>/dev/null && return 0
  local name
  for name in "$@"; do docker stats --no-stream --format "$format" "$name" 2>/dev/null || true; done
}
start_docker_stats() {
  local names=()
  mapfile -t names < <(stack_container_names)
  (
    while :; do
      docker_stats_sample "${names[@]}"
      sleep "${M8_DOCKER_STATS_TICK_S:-1}"
    done
  ) >"$1/docker-stats.ndjson" 2>"$1/docker-stats.err" &
  stats_pid=$!
  register_sampler docker-stats "$stats_pid"
}
# Ends every sampler of the run; the exit, INT and TERM trap of an arm.
stop_samplers() {
  stop_io_diag
  stop_docker_stats
  stop_waits_sampler
  stop_registered_samplers
}

# I/O diagnostics (M8_IO_DIAG=1), per second beside the arm: pg-io.sql (pg_stat_wal, pg_stat_io, pg_stat_bgwriter,
# temp_bytes) and the PostgreSQL container's cgroup io.stat and swap, read inside the container with docker exec
# (sample-cgroup-io.sh). Both land in instrument/; summarize_io_diag writes $1/io-diag.json.
io_pid=''
cgroup_pid=''
start_io_diag() {
  [ -n "${M8_IO_DIAG:-}" ] || return 0
  (export PGAPPNAME=m8-sampler-io; exec psql -X -q -v ON_ERROR_STOP=1 -At -f "$m8_dir/pg-io.sql") >"$1/pg-io.ndjson" 2>"$1/pg-io.err" &
  io_pid=$!
  register_sampler pg-io "$io_pid"
  (exec bash "$m8_dir/sample-cgroup-io.sh") >"$1/cgroup-io.txt" 2>"$1/cgroup-io.err" &
  cgroup_pid=$!
  register_sampler cgroup-io "$cgroup_pid"
}
stop_io_diag() {
  stop_sampler pg-io "$io_pid"; io_pid=''
  stop_sampler cgroup-io "$cgroup_pid"; cgroup_pid=''
}
summarize_io_diag() { # raw dir
  python3 -I "$m8_dir/io-diag-summary.py" "$1" >"$1/io-diag.json.tmp" && mv "$1/io-diag.json.tmp" "$1/io-diag.json"
}

pg_snapshot() { pg -At -f /bench/m8/pg-stats.sql >"$1.tmp" && mv "$1.tmp" "$1"; }

instrument_begin() {
  local dir="$1/instrument"
  mkdir -p "$dir"
  curl -fsS --connect-timeout 5 --max-time 10 http://127.0.0.1:8080/metrics >"$dir/serve-metrics-before.prom" ||
    die 'cannot scrape serve /metrics before the arm'
  pg -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
  pg -At -c 'SELECT pg_stat_statements_reset()' >/dev/null ||
    die 'pg_stat_statements is not preloaded; recreate PostgreSQL with run.sh up (shared_preload_libraries)'
  pg_snapshot "$dir/pg-stats-before.json"
  start_docker_stats "$dir"
  start_waits_sampler "$dir"
  start_io_diag "$dir"
}

instrument_end() {
  local dir="$1/instrument" rc=0
  kill -0 "$waits_pid" 2>/dev/null || { log "wait-event sampler exited early; see $dir/waits.err"; rc=1; }
  stop_io_diag
  stop_docker_stats
  stop_waits_sampler
  if [ -n "${M8_IO_DIAG:-}" ]; then summarize_io_diag "$1" || { log "I/O diagnostics summary failed; see $dir"; rc=1; }; fi
  curl -fsS --connect-timeout 5 --max-time 10 http://127.0.0.1:8080/metrics >"$dir/serve-metrics-after.prom" &&
    pg_snapshot "$dir/pg-stats-after.json" &&
    pg -At -f /bench/m8/pg-statements.sql >"$dir/pg-statements.json.tmp" &&
    mv "$dir/pg-statements.json.tmp" "$dir/pg-statements.json" || rc=1
  return "$rc"
}

validate_cpus() {
  python3 -I - "$M8_PG_CPUSET" "$M8_SERVE_CPUSET" "$M8_WORKER_CPUSET" <<'PY'
import re, sys
sets = []
for value in sys.argv[1:]:
    if not re.fullmatch(r'[0-9]+(-[0-9]+)?(,[0-9]+(-[0-9]+)?)*', value):
        raise SystemExit(f'invalid cpuset: {value}')
    cpus = set()
    for part in value.split(','):
        ends = [int(x) for x in part.split('-')]
        lo, hi = ends[0], ends[-1]
        if not 0 <= lo <= hi <= 65535:
            raise SystemExit(f'invalid cpuset: {value}')
        group = set(range(lo, hi + 1))
        if cpus & group:
            raise SystemExit(f'invalid cpuset (duplicate CPUs): {value}')
        cpus |= group
    if any(cpus & previous for previous in sets):
        raise SystemExit('overlapping cpusets: postgres, serve and worker+scheduler must be disjoint')
    sets.append(cpus)
PY
}

mem_bytes() {
  python3 -I - "$1" <<'PY'
import re, sys
match = re.fullmatch(r'([0-9]+)([bBkKmMgG]?)', sys.argv[1])
if not match:
    raise SystemExit(f'invalid memory limit: {sys.argv[1]} (use integer bytes, k, m or g)')
print(int(match[1]) * 1024 ** {'': 0, 'b': 0, 'k': 1, 'm': 2, 'g': 3}[match[2].lower()])
PY
}

seed_preflight() {
  local kind="$1" count bytes source completed remaining free needed
  case "$kind" in
    people) count="$M8_PEOPLE"; bytes=2048; source='M4 section 2.2: 1270 B/person + 339 B/identity; rounded for indexes/consent' ;;
    events) count="$M8_EVENTS"; bytes=512; source='M4 section 2.2: (10.1+12.1) GB / 50 M = 444 B/event, rounded up' ;;
    enrollments) count="$M8_ENROLLMENTS"; bytes=4096; source='estimate: enrollment, step, job, entry, 3 decisions, M5 staging and indexes' ;;
  esac
  log "seed $kind reaches $count rows; $bytes bytes/row ($source); 2x row bytes for WAL/temp/index headroom + 8 GiB reserve"
  completed="$(pg -At -f - <<SQL
SELECT to_regclass('m8bench.progress') IS NOT NULL AS has_progress \gset
\if :has_progress
SELECT coalesce(sum(last_n), 0) FROM m8bench.progress WHERE kind = '$kind';
\else
SELECT 0;
\endif
SQL
)"
  [[ "$completed" =~ ^[0-9]+$ ]] || die 'cannot read seed progress'
  remaining=$((count - completed))
  [ "$remaining" -ge 0 ] || die 'seed target is below committed progress; use a fresh bench volume'
  needed=$((remaining * bytes * 2 + 8589934592))
  free="$(compose exec -T postgres df -Pk /var/lib/postgresql/data | awk 'NR == 2 {print $4}')"
  [[ "$free" =~ ^[0-9]+$ ]] || die 'cannot read free disk on the Docker PostgreSQL data volume'
  log "$remaining rows remaining: need $needed bytes; Docker data volume has $((free * 1024)) bytes free"
  [ "$((free * 1024))" -ge "$needed" ] || die 'insufficient free disk on Docker data volume; seed refused'
  if [ "$kind" = enrollments ]; then
    local vm_mem
    vm_mem="$(docker info --format '{{.MemTotal}}')"
    [[ "$vm_mem" =~ ^[0-9]+$ ]] || die 'cannot check Docker VM memory before enrollment seed'
    local budget=0 limit
    for limit in "$M8_PG_MEMORY" "$M8_SERVE_MEMORY" "$M8_WORKER_MEMORY" "$M8_SCHEDULER_MEMORY"; do
      budget=$((budget + $(mem_bytes "$limit")))
    done
    [ "$vm_mem" -ge "$budget" ] || die "enrollment seed needs $budget bytes for the configured PG/app budgets in the Docker VM"
  fi
}

# The version tag the serve container runs now (an image switch during a run is followed, not refused).
running_image_tag() {
  local cid ref
  cid="$(compose ps -q zirv-flow)"
  [ -n "$cid" ] || die 'no running zirv-flow; run up first'
  ref="$(docker inspect --format '{{.Config.Image}}' "$cid")"
  image_tag_of "$ref"
}
image_tag_of() {
  local tag="${1##*:}"
  [[ "$1" == *:* && "$tag" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][a-zA-Z0-9.-]+)?$ ]] || die "image $1 is not a versioned tag"
  echo "$tag"
}

# Capture running containers, not just requested environment: old/recreated stacks are visible.
# $2 is the app image tag the containers must run (default BENCH_IMAGE_TAG); the final capture of a run
# passes the tag the containers run then, so a switch recorded by the run does not fail the check.
capture_topology() {
  local dest="$1" tag="${2:-$BENCH_IMAGE_TAG}" service cid
  : >"$dest"
  for service in postgres zirv-flow zirv-flow-worker zirv-flow-scheduler; do
    cid="$(compose ps -q "$service")"
    [ -n "$cid" ] || die "no running $service; run up first"
    docker inspect --format '{"service":"{{index .Config.Labels "com.docker.compose.service"}}","image":"{{.Config.Image}}","image_id":"{{.Image}}","cpuset":"{{.HostConfig.CpusetCpus}}","memory_bytes":{{.HostConfig.Memory}},"shm_bytes":{{.HostConfig.ShmSize}}}' "$cid" >>"$dest"
  done
  pg -At -c "SELECT json_build_object('postgres_settings', json_object_agg(name, current_setting(name))) FROM unnest(ARRAY['shared_buffers','effective_cache_size','maintenance_work_mem','max_wal_size','checkpoint_timeout','wal_compression','shared_preload_libraries','max_connections','track_io_timing','track_wal_io_timing','backend_flush_after','bgwriter_lru_maxpages','bgwriter_delay','wal_buffers']) name" >>"$dest"
  if [ -n "${M8_PG_SETTINGS:-}" ]; then
    verify_pg_side "$M8_PG_SETTINGS"
    printf '{"pg_settings_side":{"side":"%s","show":%s}}
' "$M8_PG_SETTINGS" "$(pg_side_show | tr -d '')" >>"$dest"
  fi
  python3 -I - "$dest" "$tag" "$(mem_bytes "$M8_PG_MEMORY")" "$(mem_bytes "$M8_SERVE_MEMORY")" \
    "$(mem_bytes "$M8_WORKER_MEMORY")" "$(mem_bytes "$M8_SCHEDULER_MEMORY")" <<'PY'
import json, os, sys
expected = {'postgres': 'M8_PG_CPUSET', 'zirv-flow': 'M8_SERVE_CPUSET',
            'zirv-flow-worker': 'M8_WORKER_CPUSET', 'zirv-flow-scheduler': 'M8_WORKER_CPUSET'}
tag = sys.argv[2]
memory = dict(zip(['postgres', 'zirv-flow', 'zirv-flow-worker', 'zirv-flow-scheduler'], map(int, sys.argv[3:])))
for line in open(sys.argv[1]):
    row = json.loads(line)
    if 'service' not in row:
        continue
    image = os.environ['M8_PG_IMAGE'] if row['service'] == 'postgres' else 'ghcr.io/glubiz/zirv-flow:' + tag
    if row['cpuset'] != os.environ[expected[row['service']]] or row['image'] != image:
        raise SystemExit(f"{row['service']} differs from requested image/cpuset; run up first")
    requested = memory[row['service']]
    if requested <= 0 or row['memory_bytes'] != requested:
        raise SystemExit(f"{row['service']} differs from requested memory cap; run up first")
PY
}

write_data() {
  local bench="$1" rc="$2" raw="$3" topology="$4"; shift 4
  local vm target="$out${smoke:+/smoke}/$bench${M8_DATA_SUFFIX:-}.json"
  [ "$rc" = 0 ] || target="$out${smoke:+/smoke}/$bench${M8_DATA_SUFFIX:-}.failed.json"
  vm="$(docker info --format '{{json .}}' 2>/dev/null || echo '{}')"
  python3 -I - "$target" "$bench" "$rc" "$raw" "$topology" "$vm" "$label" "${smoke:-}" "${invocation[@]}" -- "$@" <<'PY'
import datetime, json, os, pathlib, platform, subprocess, sys
path, bench, rc, raw, topology, vm, label, smoke, *args = sys.argv[1:]
cut = args.index('--')
rows = [json.loads(line) for line in pathlib.Path(topology).read_text().splitlines()]
containers = [row for row in rows if 'service' in row]
postgres_settings = next((row['postgres_settings'] for row in rows if 'postgres_settings' in row), None)
image_switch = next((row['image_switch'] for row in rows if 'image_switch' in row), None)
pg_settings_side = next((row['pg_settings_side'] for row in rows if 'pg_settings_side' in row), None)
for row in containers:
    cpus = set()
    for part in row['cpuset'].split(','):
        ends = [int(x) for x in part.split('-')]
        cpus.update(range(ends[0], ends[-1] + 1))
    row['cores'] = len(cpus)
try:
    if platform.system() == 'Darwin':
        host_memory = int(subprocess.check_output(['sysctl', '-n', 'hw.memsize'], stderr=subprocess.DEVNULL))
    elif platform.system() == 'Windows':
        host_memory = int(subprocess.check_output(['powershell.exe', '-NoProfile', '-Command',
            '(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory'], stderr=subprocess.DEVNULL))
    else:
        host_memory = os.sysconf('SC_PHYS_PAGES') * os.sysconf('SC_PAGE_SIZE')
except (OSError, ValueError, AttributeError, subprocess.CalledProcessError):
    host_memory = None
try:
    vm = json.loads(vm)
except ValueError:
    vm = {}
seed_path = pathlib.Path(os.environ['M4_STATE']).parent / 'seeds.json'
seeds = json.loads(seed_path.read_text()) if seed_path.exists() else None
results = {}
for file in sorted(pathlib.Path(raw).rglob('*.json')):
    results[str(file.relative_to(raw))] = json.loads(file.read_text())
if int(rc) == 0 and not results:
    raise SystemExit('benchmark returned no JSON results')
report = dict(benchmark=bench, label=label, ruling='ef99d178',
    limitations=['shared storage', 'Docker Desktop overhead', 'mixed core types of the dev CPU'],
    reference_hardware_validation='outstanding', image_version=os.environ['BENCH_IMAGE_TAG'],
    measurement_scope='native host tool; Docker limits do not constrain renderbench' if bench == 'render' else 'Docker app stack',
    recorded_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), smoke=bool(smoke),
    command_line=args[:cut], benchmark_command=args[cut+1:], exit_code=int(rc),
    topology=dict(containers=containers, postgres_settings=postgres_settings, pg_settings_side=pg_settings_side, image_switch=image_switch, host=dict(os=platform.system(), logical_cpus=os.cpu_count(), memory_bytes=host_memory),
        docker_vm=dict(cpus=vm.get('NCPU'), memory_bytes=vm.get('MemTotal'), operating_system=vm.get('OperatingSystem'))),
    seeds=seeds, results=results)
target = pathlib.Path(path)
target.parent.mkdir(parents=True, exist_ok=True)
temp = target.with_suffix('.json.tmp')
temp.write_text(json.dumps(report, indent=2) + '\n')
temp.replace(target)
PY
}

# Waits for the ingest inbox to empty, polling every second. Prints the seconds waited; returns 1 when the
# inbox is still not empty after $1 seconds (nothing is printed then) and 2 when the count cannot be read.
# The highest count seen goes to the file named by M8_INBOX_PEAK_FILE when that is set.
drain_inbox() {
  local cap="$1" started=$SECONDS rows peak=0
  while :; do
    rows="$(host_pg -At -c 'SELECT count(*) FROM ingest_inbox')" || return 2
    rows="${rows%$'\r'}"
    [[ "$rows" =~ ^[0-9]+$ ]] || return 2
    [ "$rows" -le "$peak" ] || peak="$rows"
    [ -z "${M8_INBOX_PEAK_FILE:-}" ] || echo "$peak" >"$M8_INBOX_PEAK_FILE"
    [ "$rows" != 0 ] || break
    [ $((SECONDS - started)) -lt "$cap" ] || return 1
    sleep 1
  done
  echo $((SECONDS - started))
}

db_now() { host_pg -At -c "SELECT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"; }

# Inbox-to-applied latency of the run that started at $2 (database UTC), from applied-latency.sql, written to
# $1/ingest-applied.json. $3 is the loadgen report. The inbox must be empty first (bounded by
# M8_APPLIED_DRAIN_SECONDS, default 600) and the measured items must equal the items the API accepted; any other
# outcome writes the file with valid=false and returns 1, so a partial set never yields a latency.
measure_applied() {
  local raw="$1" since="$2" load="$3" cap="${M8_APPLIED_DRAIN_SECONDS:-600}" waited rc=0 csv=''
  positive_integer M8_APPLIED_DRAIN_SECONDS "$cap"
  waited="$(drain_inbox "$cap")" || rc=$?
  if [ "$rc" = 0 ]; then
    csv="$(PGOPTIONS="-c bench.since=$since" host_pg -A --csv -f "${APPLIED_SQL:-$root/scripts/bench/applied-latency.sql}")" || rc=3
  fi
  python3 -I - "$raw/ingest-applied.json" "$rc" "${waited:-}" "$cap" "$since" "$load" "$csv" <<'PY'
import csv, io, json, sys
path, rc, waited, cap, since, load, rows = sys.argv[1:]
rc, cap = int(rc), int(cap)
out = dict(measure='inbox_to_applied', valid=False, since=since, target_p95_ms=5000,
           drain_wait_seconds=int(waited) if rc == 0 and waited else None, drain_cap_seconds=cap)
accepted = json.load(open(load))['total']['items_accepted']
out['items_accepted'] = accepted
if rc == 1:
    out['error'] = f'inbox did not drain within {cap} s'
elif rc == 2:
    out['error'] = 'cannot read the inbox count'
elif rc != 0:
    out['error'] = 'applied-latency.sql failed'
else:
    track = next((r for r in csv.DictReader(io.StringIO(rows)) if r['kind'] == 'track'), None)
    if track is None or not track['p95_ms']:
        out['error'] = 'no track items measured'
    elif int(track['items']) != accepted:
        out['error'] = f"measured {track['items']} track items but the API accepted {accepted}"
    else:
        out.update(valid=True, items=int(track['items']), p50_ms=float(track['p50_ms']),
                   p95_ms=float(track['p95_ms']), p99_ms=float(track['p99_ms']), max_ms=float(track['max_ms']))
        out['within_target'] = out['p95_ms'] < out['target_p95_ms']
with open(path, 'w') as f:
    json.dump(out, f, indent=2)
    f.write('\n')
sys.exit(0 if out['valid'] else 1)
PY
}

# Burst arm helpers. The arm needs the per-key limit raised the way tier.sh does (BENCH_RATE_PER_KEY and
# BENCH_BURST_PER_KEY, read by compose.bench.yaml) and restores the stack's own limits afterwards.
burst_seconds() {
  local s="${M8_BURST_SECONDS:-600}"
  [ -n "${M8_BURST_SECONDS:-}" ] || [ -z "${smoke:-${M8_SMOKE:-}}" ] || s=10
  positive_integer M8_BURST_SECONDS "$s"
  echo "$s"
}
burst_shortened() { [ "$(burst_seconds)" != 600 ]; }

limits_raised=''
recreate_serve() {
  compose up -d --no-build --no-deps --force-recreate --wait --wait-timeout 300 zirv-flow
}
raise_serve_limits() {
  limits_raised=1
  BENCH_RATE_PER_KEY=20000 BENCH_BURST_PER_KEY=40000 recreate_serve
}
restore_serve_limits() {
  [ -n "$limits_raised" ] || return 0
  limits_raised=''
  recreate_serve
}
# "rate burst" per-key limits of the running serve container.
serve_limits() {
  local cid
  cid="$(compose ps -q zirv-flow)"
  [ -n "$cid" ] || die 'no running zirv-flow; run up first'
  docker inspect --format '{{json .Config.Env}}' "$cid" | python3 -I -c '
import json, sys
env = dict(e.split("=", 1) for e in json.load(sys.stdin) if "=" in e)
print(env.get("ZIRV_FLOW_INGEST__RATE_PER_KEY", ""), env.get("ZIRV_FLOW_INGEST__BURST_PER_KEY", ""))' | tr -d '\r'
}

inbox_pid=''
start_inbox_sampler() { # csv file: epoch,rows every M8_INBOX_TICK_S (5) seconds
  (
    export PGAPPNAME=m8-sampler-inbox
    while :; do
      rows="$(host_pg -At -c 'SELECT count(*) FROM ingest_inbox')" && echo "$(date +%s),$rows" || true
      sleep "${M8_INBOX_TICK_S:-5}"
    done
  ) >"$1" 2>"$1.err" &
  inbox_pid=$!
  register_sampler inbox "$inbox_pid"
}
stop_inbox_sampler() {
  stop_sampler inbox "$inbox_pid"
  inbox_pid=''
  terminate_pg_samplers m8-sampler-inbox
}

run_exit() { stop_samplers; restore_serve_limits || log 'could not restore the serve limits'; }
