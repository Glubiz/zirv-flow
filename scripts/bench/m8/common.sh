# shellcheck shell=bash
set -euo pipefail
m8_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$m8_dir/../../.." && pwd)"
cd "$root"
label='reference-resource emulation, not a reference-hardware run'
benches=(ingest debugger explorer jobs timers decisions send render segments)
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
  out="$root/docs/benchmarks/data/m8"
}

compose() {
  docker compose -p "$M4_PROJECT" -f docker-compose.yml -f scripts/bench/compose.bench.yaml \
    -f "$M4_RESOURCE_COMPOSE" "$@"
}
pg() { compose exec -T postgres psql -X -q -U zirv -d zirv_flow_m8 -v ON_ERROR_STOP=1 "$@"; }

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
  terminate_pg_samplers
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
terminate_pg_samplers() {
  pg -At -c "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name LIKE 'm8-sampler%' AND pid <> pg_backend_pid()" >/dev/null 2>&1 ||
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
  (export PGAPPNAME=m8-sampler-waits; exec psql -X -q -v ON_ERROR_STOP=1 -At -f "$m8_dir/pg-waits.sql") >"$1/waits.ndjson" 2>"$1/waits.err" &
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
  stop_docker_stats
  stop_waits_sampler
  stop_registered_samplers
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
}

instrument_end() {
  local dir="$1/instrument" rc=0
  kill -0 "$waits_pid" 2>/dev/null || { log "wait-event sampler exited early; see $dir/waits.err"; rc=1; }
  stop_docker_stats
  stop_waits_sampler
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
  pg -At -c "SELECT json_build_object('postgres_settings', json_object_agg(name, current_setting(name))) FROM unnest(ARRAY['shared_buffers','effective_cache_size','maintenance_work_mem','max_wal_size','checkpoint_timeout','wal_compression','shared_preload_libraries','max_connections']) name" >>"$dest"
  python3 -I - "$dest" "$tag" "$(mem_bytes "$M8_PG_MEMORY")" "$(mem_bytes "$M8_SERVE_MEMORY")" \
    "$(mem_bytes "$M8_WORKER_MEMORY")" "$(mem_bytes "$M8_SCHEDULER_MEMORY")" <<'PY'
import json, os, sys
expected = {'postgres': 'M8_PG_CPUSET', 'zirv-flow': 'M8_SERVE_CPUSET',
            'zirv-flow-worker': 'M8_WORKER_CPUSET', 'zirv-flow-scheduler': 'M8_WORKER_CPUSET'}
tag = sys.argv[2]
memory = dict(zip(['postgres', 'zirv-flow', 'zirv-flow-worker', 'zirv-flow-scheduler'], map(int, sys.argv[3:])))
for line in open(sys.argv[1]):
    row = json.loads(line)
    if 'postgres_settings' in row:
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
  local vm target="$out${smoke:+/smoke}/$bench.json"
  [ "$rc" = 0 ] || target="$out${smoke:+/smoke}/$bench.failed.json"
  vm="$(docker info --format '{{json .}}' 2>/dev/null || echo '{}')"
  python3 -I - "$target" "$bench" "$rc" "$raw" "$topology" "$vm" "$label" "${smoke:-}" "${invocation[@]}" -- "$@" <<'PY'
import datetime, json, os, pathlib, platform, subprocess, sys
path, bench, rc, raw, topology, vm, label, smoke, *args = sys.argv[1:]
cut = args.index('--')
rows = [json.loads(line) for line in pathlib.Path(topology).read_text().splitlines()]
containers = [row for row in rows if 'service' in row]
postgres_settings = next((row['postgres_settings'] for row in rows if 'postgres_settings' in row), None)
image_switch = next((row['image_switch'] for row in rows if 'image_switch' in row), None)
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
    topology=dict(containers=containers, postgres_settings=postgres_settings, image_switch=image_switch, host=dict(os=platform.system(), logical_cpus=os.cpu_count(), memory_bytes=host_memory),
        docker_vm=dict(cpus=vm.get('NCPU'), memory_bytes=vm.get('MemTotal'), operating_system=vm.get('OperatingSystem'))),
    seeds=seeds, results=results)
target = pathlib.Path(path)
target.parent.mkdir(parents=True, exist_ok=True)
temp = target.with_suffix('.json.tmp')
temp.write_text(json.dumps(report, indent=2) + '\n')
temp.replace(target)
PY
}
