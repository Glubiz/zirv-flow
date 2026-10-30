#!/usr/bin/env bash
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin"
export M8_STATE="$work/state" M8_TEST_CALLS="$work/calls"
export BENCH_IMAGE_TAG=0.190.0 M8_PEOPLE=25000000 M8_EVENTS=500000000 M8_ENROLLMENTS=10000000
unset M8_SMOKE M8_PG_CPUSET M8_SERVE_CPUSET M8_WORKER_CPUSET
unset M8_PG_MEMORY M8_SERVE_MEMORY M8_WORKER_MEMORY M8_SCHEDULER_MEMORY
cat >"$work/bin/docker" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$M8_TEST_CALLS"
case "$*" in
  'info --format {{json .}}') echo '{"NCPU":32,"MemTotal":68719476736,"OperatingSystem":"Docker Desktop"}' ;;
  'info --format {{.MemTotal}}') echo "${M8_TEST_MEMORY:-68719476736}" ;;
  *'df -Pk /var/lib/postgresql/data'*) df -Pk /var/lib/postgresql/data ;;
  *'ps -q postgres'*) echo m8-postgres ;;
  *current_setting*) echo '{"postgres_settings":{"shared_buffers":"8GB","effective_cache_size":"16GB"}}' ;;
  *'psql '* ) cat >/dev/null; echo 0 ;;
  *) echo "unexpected docker invocation: $*" >&2; exit 99 ;;
esac
STUB
cat >"$work/bin/df" <<'STUB'
#!/usr/bin/env bash
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\nvolume 100 99 %s 99%% /var/lib/postgresql/data\n' "${M8_TEST_FREE_KB:-1}"
STUB
chmod +x "$work/bin/"*
export PATH="$work/bin:$PATH"
expect_failure() {
  local message="$1"; shift
  if "$@" >"$work/output" 2>&1; then
    echo "self-test FAILED: expected failure: $*" >&2; exit 1
  fi
  grep -Fq "$message" "$work/output" || { cat "$work/output" >&2; exit 1; }
}
bash "$here/run.sh" --help >"$work/help"
for name in up seed smoke ingest debugger explorer jobs timers decisions send render segments all --self-test; do
  grep -Fq -- "$name" "$work/help"
done
expect_failure 'unknown subcommand' bash "$here/run.sh" nope
expect_failure 'seed requires' bash "$here/run.sh" seed
expect_failure 'unknown seed' bash "$here/run.sh" seed nope
expect_failure 'unexpected arguments' bash "$here/run.sh" up extra
for spec in 'explorer:#395' 'decisions:#397'; do
  expect_failure "${spec#*:}" bash "$here/run.sh" "${spec%:*}"
  rc=0
  bash "$here/run.sh" "${spec%:*}" >"$work/output" 2>&1 || rc=$?
  [ "$rc" = 3 ]
done
for spec in 'M8_SERVE_CPUSET=8-11' 'M8_WORKER_CPUSET=8-11' 'M8_WORKER_CPUSET=0-3'; do
  : >"$M8_TEST_CALLS"
  expect_failure 'overlapping cpusets' env "$spec" bash "$here/run.sh" up
  [ ! -s "$M8_TEST_CALLS" ]
done
expect_failure 'invalid cpuset' env M8_PG_CPUSET=15-8 bash "$here/run.sh" up
expect_failure 'insufficient free disk' bash "$here/run.sh" seed people
if grep -Eq 'psql.*-f /bench/' "$M8_TEST_CALLS"; then
  echo 'self-test FAILED: seed refused on disk but still ran a /bench SQL file' >&2; exit 1
fi

# Pure helpers: happy cpusets, memory refusal and JSON provenance, with stubbed I/O.
# shellcheck source=run.sh
. "$here/run.sh"
smoke=''
configure
validate_cpus
export M8_TEST_FREE_KB=1000000000 M8_TEST_MEMORY=1
expect_failure 'configured PG/app budgets' bash "$here/run.sh" seed enrollments
export M8_TEST_MEMORY=68719476736
seed_preflight people >"$work/preflight" 2>&1
raw="$work/raw"; pool=1000; cmd=()
: >"$M8_TEST_CALLS"
for name in ingest segments send render debugger jobs timers; do
  cmd=()
  bench_command "$name"
  [ "${#cmd[@]}" -gt 1 ]
  case "$name" in
    debugger | jobs | timers)
      [ "${cmd[0]}" = bash ]
      [ "${cmd[1]}" = "$here/$name.sh" ]
      [ "${cmd[2]}" = "$raw" ] ;;
  esac
done
[ ! -s "$M8_TEST_CALLS" ]
expect_failure 'host psql is required' bash -c '. "$1/common.sh"; command() { return 1; }; host_pg_setup' _ "$here"
cat >"$work/bin/psql" <<'STUB'
#!/usr/bin/env bash
echo 'psql must not run in a preflight test' >&2
exit 99
STUB
chmod +x "$work/bin/psql"
expect_failure 'M8_DEBUGGER_CLIENTS must be in 1..20' env M8_DEBUGGER_CLIENTS=21 bash "$here/debugger.sh" "$raw" "$pool"
expect_failure 'M8_JOBS_CAP must be an integer' env M8_JOBS_CAP=0 bash "$here/jobs.sh" "$raw"
expect_failure 'seconds must be an integer' env M8_TIMERS_SECONDS=0 bash "$here/timers.sh" "$raw" "$pool"
[ ! -s "$M8_TEST_CALLS" ]
mkdir -p "$raw" "$M4_STATE/.." "$work/out"
out="$work/out"
printf '{"committed":{"people":5000,"events":5000,"enrollments":3000}}\n' >"$M4_STATE/../seeds.json"
printf '{"measured":true}\n' >"$raw/result.json"
printf '{"service":"postgres","image":"postgres:16.15","cpuset":"8-15","memory_bytes":34359738368}\n' >"$work/topology"
printf '{"postgres_settings":{"shared_buffers":"8GB","effective_cache_size":"16GB"}}\n' >>"$work/topology"
invocation=(bash scripts/bench/m8/run.sh ingest)
write_data ingest 0 "$raw" "$work/topology" loadgen 'argument with spaces'
python3 -I - "$out/ingest.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r['label'] == 'reference-resource emulation, not a reference-hardware run'
assert r['ruling'] == 'ef99d178' and r['reference_hardware_validation'] == 'outstanding'
assert r['topology']['containers'][0]['cores'] == 8 and len(r['topology']['containers']) == 1
assert r['topology']['postgres_settings']['shared_buffers'] == '8GB'
assert r['topology']['docker_vm']['cpus'] == 32
assert r['topology']['docker_vm']['memory_bytes'] == 68719476736
assert 'memory_bytes' in r['topology']['host']
assert r['image_version'] == '0.190.0'
assert r['seeds']['committed']['people'] == 5000
assert r['command_line'][-1] == 'ingest'
assert r['benchmark_command'] == ['loadgen', 'argument with spaces']
assert r['results']['result.json']['measured'] and r['exit_code'] == 0
PY
write_data ingest 7 "$raw" "$work/topology" failing-tool
python3 -I -c 'import json,sys; assert json.load(open(sys.argv[1]))["exit_code"] == 7' "$out/ingest.failed.json"
python3 -I -c 'import json,sys; assert json.load(open(sys.argv[1]))["exit_code"] == 0' "$out/ingest.json"
(smoke=1; write_data ingest 0 "$raw" "$work/topology" smoke-tool)
python3 -I -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["smoke"] and r["benchmark_command"] == ["smoke-tool"]' "$out/smoke/ingest.json"
(
  docker() { return 1; }
  write_data ingest 0 "$raw" "$work/topology" no-info-tool
)
python3 -I -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["topology"]["docker_vm"]["cpus"] is None' "$out/ingest.json"
rm "$raw/result.json"
expect_failure 'no JSON results' write_data ingest 0 "$raw" "$work/topology" empty-tool

# Preserve metric buckets and per-kind samples in JSON, and reject a dead sampler.
(
  curl() { printf '%s\n' '# HELP ignored' 'zirv_flow_debugger_clients 1' 'zirv_flow_debugger_delivery_seconds_bucket{le="2"} 3' 'unrelated 7'; }
  scrape_metrics unused zirv_flow_debugger_clients,zirv_flow_debugger_delivery_seconds "$raw/metrics"
  printf '%s\n' 't,kind,claims_total,completed_total,claim_lag_seconds_sum,due_ready,running,ready_lag_mean_seconds,ready_lag_max_seconds' \
    '2026-10-30T00:00:00Z,flow.step,3,2,0.5,1,0,,1.5' >"$raw/job-kinds.csv"
  kill() { return 0; }
  wait() { return 143; }
  sleep() { printf '%s\n' '2026-10-30T00:00:01Z,flow.step,4,4,0.7,0,0,,' >>"$raw/job-kinds.csv"; }
  sampler_pid=123
  stop_job_sampler "$raw"
  dead_sampler() (
    kill() { return 1; }
    wait() { return 7; }
    sampler_pid=123
    stop_job_sampler "$raw"
  )
  expect_failure 'per-kind sampler exited early (7)' dead_sampler
)
python3 -I - "$raw" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
assert json.loads((path / 'metrics.json').read_text())['samples'] == [
    'zirv_flow_debugger_clients 1', 'zirv_flow_debugger_delivery_seconds_bucket{le="2"} 3']
sample = json.loads((path / 'job-kinds.json').read_text())[0]
assert sample['kind'] == 'flow.step' and sample['claims_total'] == 3
assert sample['ready_lag_mean_seconds'] is None and sample['ready_lag_max_seconds'] == 1.5
PY

# M7 must use a prebuilt binary when M8 supplies one; a cargo invocation is a test failure.
cat >"$work/bin/cargo" <<'STUB'
#!/usr/bin/env bash
echo 'cargo must not run' >&2
exit 99
STUB
cat >"$work/bin/renderbench" <<'STUB'
#!/usr/bin/env bash
while [ "$#" -gt 0 ]; do
  if [ "$1" = --out ]; then printf '{"stub_render":true}\n' >"$2"; exit 0; fi
  shift
done
exit 1
STUB
chmod +x "$work/bin/cargo" "$work/bin/renderbench"
RENDER_BENCH_BIN="$work/bin/renderbench" bash "$root/scripts/bench/m7/render.sh" "$raw" >"$work/render-output"
grep -Fq 'stub_render' "$raw/render.json"

# The legacy callers must use the M8 resource layer and bench database, keeping their defaults.
(
  # shellcheck source=../m4/common.sh
  . "$root/scripts/bench/m4/common.sh"
  docker() {
    if [[ "$*" = *'ps -q postgres' ]]; then echo m8-postgres;
    else printf '%s\n' "$*" >>"$work/legacy"; fi
  }
  compose config
  pg -At -c 'SELECT 1'
  unset M4_RESOURCE_COMPOSE POSTGRES_DB
  compose config
  pg -At -c 'SELECT 1'
)
python3 -I - "$work/legacy" <<'PY'
import pathlib, sys
lines = pathlib.Path(sys.argv[1]).read_text().splitlines()
assert 'compose.m8.yaml' in lines[0] and 'compose.tiers.yaml' not in lines[0]
assert '-d zirv_flow_m8 ' in lines[1]
assert 'compose.tiers.yaml' in lines[2] and 'compose.m4.yaml' in lines[2]
assert '-d zirv_flow ' in lines[3]
PY
echo 'm8 self-test: parsing, registration, cpusets, disk/memory guards, metadata and prebuilt render passed'
