# shellcheck shell=bash
set -euo pipefail
m8_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$m8_dir/../../.." && pwd)"
cd "$root"
label='reference-resource emulation, not a reference-hardware run'
benches=(ingest debugger explorer jobs timers decisions send render segments)
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

# Capture running containers, not just requested environment: old/recreated stacks are visible.
capture_topology() {
  local dest="$1" service cid
  : >"$dest"
  for service in postgres zirv-flow zirv-flow-worker zirv-flow-scheduler; do
    cid="$(compose ps -q "$service")"
    [ -n "$cid" ] || die "no running $service; run up first"
    docker inspect --format '{"service":"{{index .Config.Labels "com.docker.compose.service"}}","image":"{{.Config.Image}}","image_id":"{{.Image}}","cpuset":"{{.HostConfig.CpusetCpus}}","memory_bytes":{{.HostConfig.Memory}},"shm_bytes":{{.HostConfig.ShmSize}}}' "$cid" >>"$dest"
  done
  pg -At -c "SELECT json_build_object('postgres_settings', json_object_agg(name, current_setting(name))) FROM unnest(ARRAY['shared_buffers','effective_cache_size','maintenance_work_mem','max_wal_size','checkpoint_timeout']) name" >>"$dest"
  python3 -I - "$dest" "$(mem_bytes "$M8_PG_MEMORY")" "$(mem_bytes "$M8_SERVE_MEMORY")" \
    "$(mem_bytes "$M8_WORKER_MEMORY")" "$(mem_bytes "$M8_SCHEDULER_MEMORY")" <<'PY'
import json, os, sys
expected = {'postgres': 'M8_PG_CPUSET', 'zirv-flow': 'M8_SERVE_CPUSET',
            'zirv-flow-worker': 'M8_WORKER_CPUSET', 'zirv-flow-scheduler': 'M8_WORKER_CPUSET'}
memory = dict(zip(['postgres', 'zirv-flow', 'zirv-flow-worker', 'zirv-flow-scheduler'], map(int, sys.argv[2:])))
for line in open(sys.argv[1]):
    row = json.loads(line)
    if 'postgres_settings' in row:
        continue
    image = os.environ['M8_PG_IMAGE'] if row['service'] == 'postgres' else 'ghcr.io/glubiz/zirv-flow:' + os.environ['BENCH_IMAGE_TAG']
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
    topology=dict(containers=containers, postgres_settings=postgres_settings, host=dict(os=platform.system(), logical_cpus=os.cpu_count(), memory_bytes=host_memory),
        docker_vm=dict(cpus=vm.get('NCPU'), memory_bytes=vm.get('MemTotal'), operating_system=vm.get('OperatingSystem'))),
    seeds=seeds, results=results)
target = pathlib.Path(path)
target.parent.mkdir(parents=True, exist_ok=True)
temp = target.with_suffix('.json.tmp')
temp.write_text(json.dumps(report, indent=2) + '\n')
temp.replace(target)
PY
}
