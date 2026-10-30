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
for spec in 'decisions:#397'; do
  rc=0
  bash "$here/run.sh" "${spec%:*}" >"$work/output" 2>&1 || rc=$?
  [ "$rc" = 3 ]
  grep -Fq -- "${spec#*:}" "$work/output"
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
for name in ingest segments send render debugger explorer jobs timers; do
  cmd=()
  bench_command "$name"
  [ "${#cmd[@]}" -gt 1 ]
  case "$name" in
    debugger | explorer | jobs | timers)
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
expect_failure 'seconds must be an integer' env M8_SECONDS=0 bash "$here/explorer.sh" "$raw" "$pool"
expect_failure 'M8_JOBS_CAP must be an integer' env M8_JOBS_CAP=0 bash "$here/jobs.sh" "$raw"
expect_failure 'must be below M8_JOBS_CONCURRENCY (16)' env M8_JOBS_CAP=16 bash "$here/jobs.sh" "$raw"
expect_failure 'must be below M8_JOBS_CONCURRENCY (4)' env M8_JOBS_CAP=4 M8_JOBS_CONCURRENCY=4 bash "$here/jobs.sh" "$raw"
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

# Preflight names a missing sampler tool; run_all continues past a failed bench and fails at the end.
expect_failure 'jq is required by the per-kind sampler' bash -c '. "$1/common.sh"; command() { return 1; }; require_job_sampler' _ "$here"
(
  bash() { printf '%s\n' "$2" >>"$work/ran"; [ "$2" != jobs ]; }
  rc=0
  run_all >"$work/all.out" 2>&1 || rc=$?
  [ "$rc" = 1 ]
)
grep -Fxq timers "$work/ran"
grep -Fq 'failed: jobs;' "$work/all.out"

# A backgrounded track_load must be loadgen itself, so kill in a cleanup trap stops it.
cat >"$work/bin/loadgen" <<'STUB'
#!/usr/bin/env bash
echo $$ >"$M8_TEST_LOADGEN_PID"
exec sleep 30
STUB
chmod +x "$work/bin/loadgen"
(
  export LOADGEN="$work/bin/loadgen" M8_TEST_LOADGEN_PID="$work/loadgen.pid"
  track_load 1 1 "$work/ingest.json" &
  load_pid=$!
  until [ -s "$work/loadgen.pid" ]; do sleep 0.1; done
  [ "$(cat "$work/loadgen.pid")" = "$load_pid" ]
  kill "$load_pid"
  wait "$load_pid" || true
  ! kill -0 "$load_pid" 2>/dev/null
)

# Explorer orchestration with stubbed HTTP/SQL/tools: both budgets, concurrency and cleanup.
cat >"$work/bin/explorer-loadgen" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$M8_EXPLORER_CALLS"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --out ]; then
    printf '{"valid":true,"shapes":[{"latency":{"count":1,"p95_ms":2,"p99_ms":3},"outcomes":{"query_timeouts":0}}]}\n' >"$2"
    exit "${M8_EXPLORER_FAIL:-0}"
  fi
  shift
done
exit 99
STUB
chmod +x "$work/bin/explorer-loadgen"
explorer_stub_run() (
  export LOADGEN="$work/bin/explorer-loadgen" M8_SECONDS=2
  export M8_EXPLORER_CALLS="$work/explorer-calls"
  # shellcheck source=explorer.sh
  . "$here/explorer.sh"
  host_pg_setup() { :; }
  login() { :; }
  state_get() { echo 00000000-0000-0000-0000-000000000001; }
  mkdir -p "$M4_STATE"
  touch "$M4_STATE/explorer.key"
  printf 'synthetic-key\n' >"$M4_STATE/explorer.key"
  host_pg() {
    if [[ "$*" = *'explorer.sql'* ]]; then
      printf '[{"Plan":{"Node Type":"Aggregate"}}]\n'
    else
      cat >/dev/null
      printf '%s\n' '{"retained_rows":50,"workspace_window_rows":10,"person_exists":true,"person_id":"00000000-0000-0000-0000-000000000002","from":"2026-08-01T00:00:00Z","to":"2026-10-30T00:00:00Z"}'
    fi
  }
  wait_inbox() { :; }
  compose() {
    printf '%s\n' "$*" >>"$work/explorer-compose"
    if [[ "$*" = *'ps -q'* ]]; then echo serve; fi
  }
  docker() {
    if [ "$setting" = default ]; then echo '[]';
    else echo '["ZIRV_FLOW_EXPLORER__INTERACTIVE_TIMEOUT_MS=15000"]'; fi
  }
  curl() {
    printf '%s\n' 'zirv_flow_explorer_query_seconds_bucket{kind="count",le="5"} 1' 'zirv_flow_explorer_timeouts_total{kind="count"} 2'
  }
  track_load() {
    printf '{"total":{"latency_from_schedule":{"p99_ms":7},"achieved_rate":2000}}\n' >"$3"
  }
  explorer_main "$1" 1000
)
: >"$work/explorer-calls"; : >"$work/explorer-compose"
explorer_stub_run "$work/full-explorer" >"$work/explorer-output" 2>&1
python3 -I - "$work" <<'PYTEST'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
calls = (path / 'explorer-calls').read_text().splitlines()
assert len(calls) == 6
assert [c.split('--clients ')[1].split()[0] for c in calls] == ['1','4','16'] * 2
root = path / 'full-explorer/explorer'
assert json.loads((root / 'timeout-default/config.json').read_text())['interactive_timeout_ms'] == 5000
assert json.loads((root / 'timeout-15000/config.json').read_text())['interactive_timeout_ms'] == 15000
for setting in ('default', '15000'):
    for n in (1,4,16):
        r = json.loads((root / f'timeout-{setting}/clients-{n}/comparison.json').read_text())
        assert r['scale']['retained_rows'] == 50
        assert r['ingest_p99_alone_ms'] == r['ingest_p99_with_explorer_ms'] == 7
        assert r['queries']['shapes'][0]['outcomes']['query_timeouts'] == 0
        metrics = json.loads((root / f'timeout-{setting}/clients-{n}/metrics-after.json').read_text())['samples']
        assert len(metrics) == 2 and 'bucket' in metrics[0] and 'timeouts_total' in metrics[1]
assert len(list(root.glob('*.plan'))) == 7
assert json.loads((root / 'export.json').read_text())['plans']
assert (path / 'explorer-compose').read_text().splitlines()[-1] == 'up -d --no-build --no-deps --force-recreate zirv-flow'
PYTEST
: >"$work/explorer-calls"
(export M8_SMOKE=1; explorer_stub_run "$work/smoke-explorer") >"$work/explorer-output" 2>&1
python3 -I - "$work/explorer-calls" <<'PYTEST'
import pathlib, sys
calls = pathlib.Path(sys.argv[1]).read_text().splitlines()
assert len(calls) == 2
assert all('--clients 1 --duration 5' in c for c in calls)
PYTEST
# Running in a fresh shell preserves errexit while expect_failure observes the exit status.
export -f explorer_stub_run
export work here
rc=0
env M8_EXPLORER_FAIL=7 bash -c 'explorer_stub_run "$work/failed-explorer"' >"$work/explorer-output" 2>&1 || rc=$?
[ "$rc" = 7 ]
[ "$(tail -n 1 "$work/explorer-compose")" = 'up -d --no-build --no-deps --force-recreate zirv-flow' ]
unset -f explorer_stub_run

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
