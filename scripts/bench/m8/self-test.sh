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
for name in up seed smoke ingest burst debugger explorer jobs timers decisions send render segments all --self-test; do
  grep -Fq -- "$name" "$work/help"
done
expect_failure 'unknown subcommand' bash "$here/run.sh" nope
expect_failure 'seed requires' bash "$here/run.sh" seed
expect_failure 'unknown seed' bash "$here/run.sh" seed nope
expect_failure 'unexpected arguments' bash "$here/run.sh" up extra
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
for name in ingest burst segments send render debugger explorer jobs timers decisions; do
  cmd=()
  bench_command "$name"
  [ "${#cmd[@]}" -gt 1 ]
  case "$name" in
    burst | debugger | explorer | jobs | timers)
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
mkdir -p "$work/nolinux"
expect_failure 'no Linux loadgen' env M8_DEBUGGER_CONTAINER=1 M8_STATE="$work/nolinux" bash "$here/debugger.sh" "$raw" "$pool"
grep -Fq 'container_mode' "$here/debugger-latency.sql"
grep -Fq 'container_mode' "$here/debugger-clock-sample.sql"
grep -Fq 'offset_before_us' "$here/debugger-latency.sql"
grep -Fq 'container_mode' "$here/debugger-clock-bracket.sql"
if grep -Fq 'run_clock_step' "$here/debugger-latency.sql"; then echo 'self-test FAILED: run-step gate still present' >&2; exit 1; fi
for setting in wal_compression=lz4 shared_preload_libraries=pg_stat_statements max_wal_size='${M8_MAX_WAL_SIZE:-32GB}' max_connections='${M8_PG_MAX_CONNECTIONS:-400}' ZIRV_FLOW_DATABASE__MAX_CONNECTIONS; do
  grep -Fq -- "$setting" "$here/compose.m8.yaml" || { echo "self-test FAILED: compose.m8.yaml lacks $setting" >&2; exit 1; }
done
bash -n "$here/ingest.sh"
bash -n "$here/burst.sh"
expect_failure 'seconds must be an integer' env M8_SECONDS=0 bash "$here/explorer.sh" "$raw" "$pool"
expect_failure 'M8_JOBS_CAP must be an integer' env M8_JOBS_CAP=0 bash "$here/jobs.sh" "$raw"
expect_failure 'must be below M8_JOBS_CONCURRENCY (16)' env M8_JOBS_CAP=16 bash "$here/jobs.sh" "$raw"
expect_failure 'must be below M8_JOBS_CONCURRENCY (4)' env M8_JOBS_CAP=4 M8_JOBS_CONCURRENCY=4 bash "$here/jobs.sh" "$raw"
expect_failure 'seconds must be an integer' env M8_TIMERS_SECONDS=0 bash "$here/timers.sh" "$raw" "$pool"
expect_failure 'worker_concurrency must be an integer' env M8_WORKER_CONCURRENCY=0 bash "$here/timers.sh" "$raw" "$pool"
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
    if [ "$setting" = default ]; then echo '["PATH=/bin","ZIRV_FLOW_EXPLORER__INTERACTIVE_TIMEOUT_MS"]';
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
# Shapes reach the loadgen; container mode needs the Linux loadgen.
: >"$work/explorer-calls"
(export M8_SMOKE=1 M8_EXPLORER_SHAPES=export; explorer_stub_run "$work/shapes-explorer") >"$work/explorer-output" 2>&1
grep -Fq -- '--shapes export' "$work/explorer-calls"
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
# Samplers: a registered live sampler blocks the next run; stopping it clears the registry.
leftover() ( assert_no_samplers </dev/null )
(
  command sleep 60 &
  pid=$!
  register_sampler waits "$pid"
  expect_failure 'sampler of an earlier run is still alive' leftover
  stop_sampler waits "$pid"
  leftover
  ! kill -0 "$pid" 2>/dev/null
  [ -z "$(ls -A "$(sampler_dir)")" ]
)
# docker stats is keyed by container name, one JSON line per container, and a missing container costs only its own line.
(
  docker() { printf '%s\n' "$*"; }
  docker_stats_sample zirv-flow-m8-postgres-1 zirv-flow-m8-zirv-flow-1 >"$work/stats-one"
  [ "$(wc -l <"$work/stats-one")" = 1 ]
  grep -Fq '"name":"{{.Name}}"' "$work/stats-one"
  grep -Fq 'zirv-flow-m8-postgres-1 zirv-flow-m8-zirv-flow-1' "$work/stats-one"
  docker() { if [ "$#" -gt 5 ]; then return 1; fi; printf '%s\n' "$5"; }
  docker_stats_sample zirv-flow-m8-postgres-1 zirv-flow-m8-zirv-flow-1 >"$work/stats-two"
  [ "$(cat "$work/stats-two")" = "$(printf 'zirv-flow-m8-postgres-1\nzirv-flow-m8-zirv-flow-1')" ]
  [ "$(M4_PROJECT=p stack_container_names | head -n 1)" = p-postgres-1 ]
)
# An image switch during a run is followed and recorded; the check itself stays (capture_topology takes the tag).
[ "$(image_tag_of ghcr.io/glubiz/zirv-flow:0.225.0)" = 0.225.0 ]
expect_failure 'is not a versioned tag' bash -c '. "$1/common.sh"; image_tag_of ghcr.io/glubiz/zirv-flow:latest' _ "$here"
(
  running_image_tag() { echo 0.225.0; }
  capture_topology() { [ "$2" = 0.225.0 ]; }
  BENCH_IMAGE_TAG=0.224.0
  : >"$work/switch-topology"
  finish_topology "$work/switch-topology"
  [ "$BENCH_IMAGE_TAG" = 0.225.0 ]
  grep -Fq '{"image_switch":{"from":"0.224.0","to":"0.225.0"}}' "$work/switch-topology"
  : >"$work/same-topology"
  finish_topology "$work/same-topology"
  [ ! -s "$work/same-topology" ]
)
# The send audience is bounded by the first workspace's people.
expect_failure 'exceeds the first workspace' bash -c '. "$1/run.sh"; configure; raw=/nonexistent; pool=1000; PEOPLE=2000; prepare_send' _ "$here"

# Burst arm and applied latency (#397), with stub psql, loadgen and docker: no stack.
burst_seconds_of() ( unset M8_BURST_SECONDS; [ -z "${1:-}" ] || M8_BURST_SECONDS="$1"; smoke="${2:-}"; burst_seconds )
[ "$(burst_seconds_of)" = 600 ] && [ "$(burst_seconds_of 30)" = 30 ] && [ "$(burst_seconds_of '' 1)" = 10 ]
expect_failure 'M8_BURST_SECONDS must be an integer' env M8_BURST_SECONDS=0 bash -c '. "$1/common.sh"; burst_seconds' _ "$here"
(unset M8_BURST_SECONDS; ! burst_shortened)
(export M8_BURST_SECONDS=600; ! burst_shortened)
(export M8_BURST_SECONDS=30; burst_shortened)
# run_bench marks a shortened burst as smoke, raises the key limit before the arm and restores it after.
burst_run() ( # $1 = M8_BURST_SECONDS, $2 = log file
  export M8_BURST_SECONDS="$1"
  M8_RESULT="$2"
  smoke=''; M4_STATE="$work/rb-state"; M8_STATE="$work/rb-state"; out="$work/rb-out"; mkdir -p "$M4_STATE/.."
  host_pg_setup() { :; }; validate_cpus() { :; }; seed_manifest() { :; }; capture_topology() { :; }
  assert_no_samplers() { :; }; quiet_start() { :; }; instrument_begin() { :; }; instrument_end() { :; }
  finish_topology() { :; }; run_exit() { :; }
  raise_serve_limits() { echo raise >>"$M8_RESULT"; }
  restore_serve_limits() { echo restore >>"$M8_RESULT"; }
  write_data() { echo "write smoke=${smoke:-} rc=$2" >>"$M8_RESULT"; }
  bash() { [ "$1" != arm ] || echo arm >>"$M8_RESULT"; }
  bench_command() { cmd=(bash arm); }
  run_bench burst
)
rm -f "$work/rb-short" "$work/rb-full"
burst_run 30 "$work/rb-short"
[ "$(paste -sd, "$work/rb-short")" = 'raise,arm,restore,write smoke=1 rc=0' ]
burst_run 600 "$work/rb-full"
[ "$(paste -sd, "$work/rb-full")" = 'raise,arm,restore,write smoke= rc=0' ]

cat >"$work/bin/docker" <<'STUB'
#!/usr/bin/env bash
case "$*" in
  *'ps -q zirv-flow'*) echo m8-serve ;;
  'inspect --format {{json .Config.Env}} m8-serve') echo "$M8_TEST_SERVE_ENV" ;;
  *'psql '*) cat >/dev/null; echo 0 ;;
  *) echo "unexpected docker invocation: $*" >&2; exit 99 ;;
esac
STUB
cat >"$work/bin/psql" <<'STUB'
#!/usr/bin/env bash
case "$*" in
  *to_char*) echo 2026-11-04T10:00:00.000000Z ;;
  *'count(*) FROM ingest_inbox'*)
    if [ "${PGAPPNAME:-}" = m8-sampler-inbox ]; then echo 7; exit 0; fi
    v="$(head -n1 "$M8_TEST_INBOX_SEQ")"
    if [ "$(wc -l <"$M8_TEST_INBOX_SEQ")" -gt 1 ]; then sed -i 1d "$M8_TEST_INBOX_SEQ"; fi
    echo "$v" ;;
  *-f*) echo applied >>"$M8_TEST_APPLIED_CALLS"; cat "$M8_TEST_APPLIED_CSV" ;;
  *) echo "unexpected psql: $*" >&2; exit 99 ;;
esac
STUB
cat >"$work/bin/stub-loadgen" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$M8_TEST_LOADGEN_ARGS"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --out ]; then cp "$M8_TEST_LOAD_JSON" "$2"; exit "${M8_TEST_LOADGEN_RC:-0}"; fi
  shift
done
exit 99
STUB
chmod +x "$work/bin/docker" "$work/bin/psql" "$work/bin/stub-loadgen"
export LOADGEN="$work/bin/stub-loadgen" M8_TEST_LOADGEN_ARGS="$work/loadgen-args" M8_TEST_INBOX_SEQ="$work/inbox-seq"
export M8_TEST_APPLIED_CALLS="$work/applied-calls" M8_TEST_APPLIED_CSV="$work/applied.csv" M8_TEST_LOAD_JSON="$work/load.json"
export M8_INBOX_TICK_S=1
printf 'kind,items,p50_ms,p95_ms,p99_ms,max_ms\nidentify,0,,,,\ntrack,1200,10.0,40.5,90.1,200.0\n' >"$work/applied.csv"
printf '%s\n' '{"total":{"achieved_rate":10000.0,"items_offered":6000000,"items_accepted":1200,
  "status":{"accepted_202":60000,"rate_limited_429":0,"server_5xx":0,"other":0,"transport_errors":0},
  "gave_up_after_retries":0,"abandoned_requests":0,
  "latency_from_schedule":{"count":60000,"p50_ms":3.0,"p95_ms":9.0,"p99_ms":20.0,"max_ms":80.0}}}' >"$work/load.json"
export M8_TEST_SERVE_ENV='["PATH=/bin","ZIRV_FLOW_INGEST__RATE_PER_KEY=20000","ZIRV_FLOW_INGEST__BURST_PER_KEY=40000"]'
json_of() { python3 -I -c 'import json,sys; r=json.load(open(sys.argv[1])); exec(sys.argv[2])' "$1" "$2"; }

# Ingest arm: the applied latency lands in ingest-applied.json beside the loadgen report.
mkdir -p "$work/ing-ok"; printf '0\n' >"$M8_TEST_INBOX_SEQ"; : >"$M8_TEST_APPLIED_CALLS"
bash "$here/ingest.sh" "$work/ing-ok" 1000 >/dev/null 2>&1
json_of "$work/ing-ok/ingest-applied.json" "assert r['valid'] and r['items']==1200 and r['p95_ms']==40.5 and r['p99_ms']==90.1 and r['within_target'] and r['drain_wait_seconds']==0 and r['since']=='2026-11-04T10:00:00.000000Z'"
grep -Fq -- '--mode track' "$M8_TEST_LOADGEN_ARGS"
# A measured set that differs from the accepted items is partial: no latency is reported.
mkdir -p "$work/ing-partial"; printf '0\n' >"$M8_TEST_INBOX_SEQ"
sed 's/"items_accepted":1200/"items_accepted":1300/' "$work/load.json" >"$work/load-partial.json"
(export M8_TEST_LOAD_JSON="$work/load-partial.json"
 expect_failure 'inbox-to-applied latency not measured' bash "$here/ingest.sh" "$work/ing-partial" 1000)
json_of "$work/ing-partial/ingest-applied.json" "assert not r['valid'] and 'p95_ms' not in r and 'accepted 1300' in r['error']"
# A drain that does not finish in its bound fails the run and never queries the latency.
mkdir -p "$work/ing-stuck"; printf '5\n' >"$M8_TEST_INBOX_SEQ"; : >"$M8_TEST_APPLIED_CALLS"
(export M8_APPLIED_DRAIN_SECONDS=2
 expect_failure 'inbox-to-applied latency not measured' bash "$here/ingest.sh" "$work/ing-stuck" 1000)
json_of "$work/ing-stuck/ingest-applied.json" "assert not r['valid'] and r['drain_wait_seconds'] is None and r['drain_cap_seconds']==2 and 'did not drain' in r['error']"
[ ! -s "$M8_TEST_APPLIED_CALLS" ]
# The batch arm measures no applied latency.
mkdir -p "$work/ing-batch"; printf '0\n' >"$M8_TEST_INBOX_SEQ"
M8_BATCH_SIZE=500 bash "$here/ingest.sh" "$work/ing-batch" 1000 >/dev/null 2>&1
[ ! -e "$work/ing-batch/ingest-applied.json" ]

# Burst arm: load flags, drain timing, backlog peak, pass/fail against 15 minutes, failure on a drain timeout.
mkdir -p "$work/burst-ok"; : >"$M8_TEST_LOADGEN_ARGS"; printf '5\n3\n0\n' >"$M8_TEST_INBOX_SEQ"
bash "$here/burst.sh" "$work/burst-ok" 1000 >/dev/null 2>&1
grep -Fq -- '--mode batch --batch-size 100 --burst-rate 10000 --burst-duration 600 --people 1000' "$M8_TEST_LOADGEN_ARGS"
json_of "$work/burst-ok/burst.json" "assert r['measure']=='burst' and not r['shortened'] and r['burst_seconds']==600 and r['achieved_rate']==10000.0 and r['api_latency_ms']['p99_ms']==20.0 and r['rejected_or_failed']['total']==0 and r['inbox_backlog_peak_rows']==7 and 2<=r['drain_seconds']<=4 and not r['drain_timed_out'] and r['drain_within_15_min'] and r['rate_held'] and r['passed'] and r['rate_per_key']==20000 and r['burst_per_key']==40000"
# Rejected requests show in the data and break rate_held.
mkdir -p "$work/burst-429"; printf '0\n' >"$M8_TEST_INBOX_SEQ"
sed 's/"rate_limited_429":0/"rate_limited_429":9/' "$work/load.json" >"$work/load-429.json"
(export M8_TEST_LOAD_JSON="$work/load-429.json"; bash "$here/burst.sh" "$work/burst-429" 1000 >/dev/null 2>&1)
json_of "$work/burst-429/burst.json" "assert r['rejected_or_failed']['total']==9 and not r['rate_held'] and not r['passed'] and r['drain_within_15_min']"
# A drain past the cap is a failed run; the data records the timeout and no drain time.
mkdir -p "$work/burst-stuck"; printf '5\n' >"$M8_TEST_INBOX_SEQ"
(export M8_BURST_DRAIN_CAP_SECONDS=2
 expect_failure 'did not drain within 2s' bash "$here/burst.sh" "$work/burst-stuck" 1000)
json_of "$work/burst-stuck/burst.json" "assert r['drain_timed_out'] and r['drain_seconds'] is None and not r['drain_within_15_min'] and not r['passed'] and r['inbox_backlog_peak_rows']==7"
# A shortened burst says so and runs for the shortened time.
mkdir -p "$work/burst-short"; : >"$M8_TEST_LOADGEN_ARGS"; printf '0\n' >"$M8_TEST_INBOX_SEQ"
M8_BURST_SECONDS=20 bash "$here/burst.sh" "$work/burst-short" 1000 >/dev/null 2>&1
grep -Fq -- '--burst-duration 20 ' "$M8_TEST_LOADGEN_ARGS"
json_of "$work/burst-short/burst.json" "assert r['shortened'] and r['burst_seconds']==20"
# Under M8_SMOKE (the child arm does not see run.sh's smoke variable) the burst defaults to 10 seconds.
mkdir -p "$work/burst-smoke"; : >"$M8_TEST_LOADGEN_ARGS"; printf '0\n' >"$M8_TEST_INBOX_SEQ"
(unset M8_BURST_SECONDS; M8_SMOKE=1 bash "$here/burst.sh" "$work/burst-smoke" 1000 >/dev/null 2>&1)
grep -Fq -- '--burst-duration 10 ' "$M8_TEST_LOADGEN_ARGS"
json_of "$work/burst-smoke/burst.json" "assert r['shortened'] and r['burst_seconds']==10"
# The arm refuses a serve container that still has the default limits, and a failing loadgen fails the arm.
mkdir -p "$work/burst-limits"; : >"$M8_TEST_LOADGEN_ARGS"
(export M8_TEST_SERVE_ENV='["ZIRV_FLOW_INGEST__RATE_PER_KEY=10000","ZIRV_FLOW_INGEST__BURST_PER_KEY=20000"]'
 expect_failure 'not the raised 20000/40000' bash "$here/burst.sh" "$work/burst-limits" 1000)
[ ! -s "$M8_TEST_LOADGEN_ARGS" ]
mkdir -p "$work/burst-rc"
(export M8_TEST_LOADGEN_RC=3; expect_failure 'loadgen exited 3' bash "$here/burst.sh" "$work/burst-rc" 1000)
[ ! -e "$work/burst-rc/burst.json" ]
# Both arms' data reaches the data JSON through write_data (results hold the files under the raw directory).
(smoke=1; write_data burst 0 "$work/burst-short" "$work/topology" bash burst.sh)
python3 -I -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["smoke"] and r["results"]["burst.json"]["shortened"] and r["exit_code"]==0' "$out/smoke/burst.json"
write_data ingest 0 "$work/ing-ok" "$work/topology" bash ingest.sh
python3 -I -c 'import json,sys; r=json.load(open(sys.argv[1])); assert not r["smoke"] and r["results"]["ingest-applied.json"]["valid"]' "$out/ingest.json"
write_data burst 1 "$work/burst-stuck" "$work/topology" bash burst.sh
python3 -I -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["exit_code"]==1' "$out/burst.failed.json"

# PostgreSQL settings A/B (#397), with stubs: the side switch, the SHOW check, the ABAB driver, the diagnostics summary.
for var in M8_PG_TRACK_IO_TIMING M8_PG_BACKEND_FLUSH_AFTER M8_PG_BGWRITER_MAXPAGES M8_PG_BGWRITER_DELAY M8_PG_WAL_BUFFERS; do
  grep -Fq -- "\${$var:-" "$here/compose.m8.yaml" || { echo "self-test FAILED: compose.m8.yaml lacks $var" >&2; exit 1; }
done
(pg_side_env b; [ "$M8_PG_BACKEND_FLUSH_AFTER" = 256kB ] && [ "$M8_PG_BGWRITER_MAXPAGES" = 1000 ] && [ "$M8_PG_BGWRITER_DELAY" = 50ms ] &&
  [ "$M8_PG_WAL_BUFFERS" = 64MB ] && [ "$M8_PG_TRACK_IO_TIMING" = on ])
(pg_side_env a; [ "$M8_PG_BACKEND_FLUSH_AFTER" = 0 ] && [ "$M8_PG_BGWRITER_MAXPAGES" = 100 ] && [ "$M8_PG_WAL_BUFFERS" = -1 ] && [ "$M8_PG_TRACK_IO_TIMING" = on ])
expect_failure 'M8_PG_SETTINGS must be a or b' bash -c '. "$1/common.sh"; pg_side_env c' _ "$here"
show_a='{"track_io_timing":"on","track_wal_io_timing":"on","backend_flush_after":"0","bgwriter_lru_maxpages":"100","bgwriter_delay":"200ms","wal_buffers":"16MB"}'
show_b='{"track_io_timing":"on","track_wal_io_timing":"on","backend_flush_after":"256kB","bgwriter_lru_maxpages":"1000","bgwriter_delay":"50ms","wal_buffers":"64MB"}'
(pg_side_show() { echo "$show_a"; }; verify_pg_side a)
(pg_side_show() { echo "$show_b"; }; verify_pg_side b)
# A setting that did not take effect (wal_buffers needs the restart) fails the run before the arm.
expect_failure "wal_buffers is '16MB', expected '64MB'" \
  env SHOW="$show_a" bash -c '. "$1/run.sh"; pg_side_show() { echo "$SHOW"; }; verify_pg_side b' _ "$here"
expect_failure 'did not take effect' env SHOW="$show_b" bash -c '. "$1/run.sh"; pg_side_show() { echo "$SHOW"; }; verify_pg_side a' _ "$here"
# The switch recreates only postgres, keeps volumes, and checks SHOW afterwards.
switch_calls="$work/switch-calls"; : >"$switch_calls"
(
  compose() { printf '%s\n' "$*" >>"$switch_calls"; }
  pg() { [ "$*" = "-At -c SELECT 1" ] && echo 1; return 0; }
  curl() { return 0; }
  pg_side_show() { echo "$show_b"; }
  switch_pg_settings b
)
grep -Fq -- 'up -d --no-build --no-deps --force-recreate' "$switch_calls"
[ "$(grep -c . "$switch_calls")" = 1 ] && [ "$(awk '{print $NF}' "$switch_calls")" = postgres ]
if grep -Eq -- 'down|rm |volume| -v' "$switch_calls"; then echo 'self-test FAILED: the switch touched volumes or other containers' >&2; exit 1; fi
expect_failure 'did not take effect' env SHOW="$show_a" bash -c '. "$1/run.sh"; compose() { :; }; pg() { [ "$*" = "-At -c SELECT 1" ] && echo 1; return 0; }
  curl() { return 0; }; pg_side_show() { echo "$SHOW"; }; switch_pg_settings b' _ "$here"
# The wait sampler is one sampler: M8_IO_DIAG makes it keyed and 10 Hz, otherwise it keeps its 1 Hz form.
grep -Fq 'keyed' "$here/pg-waits.sql" && grep -Fq 'application_name, backend_type' "$here/pg-waits.sql" && grep -Fq '\watch :interval' "$here/pg-waits.sql"
mkdir -p "$work/waits-bin" "$work/waits-a" "$work/waits-b"
cat >"$work/waits-bin/psql" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >"$M8_TEST_PSQL_ARGS"
STUB
chmod +x "$work/waits-bin/psql"
(
  export PATH="$work/waits-bin:$PATH" M8_TEST_PSQL_ARGS="$work/waits-args-a"
  unset M8_IO_DIAG
  start_waits_sampler "$work/waits-a"; wait "$waits_pid" || true; stop_waits_sampler
  export M8_TEST_PSQL_ARGS="$work/waits-args-b" M8_IO_DIAG=1
  start_waits_sampler "$work/waits-b"; wait "$waits_pid" || true; stop_waits_sampler
)
! grep -Fq 'keyed' "$work/waits-args-a"
grep -Fq -- '-v interval=0.1 -v keyed=1' "$work/waits-args-b"
# The diagnostics summary: slow seconds, WAL sync per sync, writeback bytes, cgroup writes and swap, keyed waits.
dg="$work/diag"; mkdir -p "$dg/instrument"
cat >"$dg/ingest.json" <<'JSON'
{"total":{"latency_from_schedule":{"p99_ms":300.0}},"timeline_from_schedule":[{"second":0,"max_ms":10.0},{"second":1,"max_ms":263.0},{"second":2,"max_ms":262.0},{"second":3,"max_ms":900.0}]}
JSON
cat >"$dg/instrument/pg-io.ndjson" <<'JSON'
{"t":"a","wal":{"wal_write":10,"wal_sync":10,"wal_write_time":5,"wal_sync_time":20,"wal_buffers_full":0},"io":[{"backend_type":"client backend","context":"normal","object":"relation","writes":5,"writebacks":0,"fsyncs":0,"op_bytes":8192}],"bgwriter":{"maxwritten_clean":0,"buffers_clean":0},"temp_bytes":0}
{"t":"b","wal":{"wal_write":20,"wal_sync":14,"wal_write_time":6,"wal_sync_time":40,"wal_buffers_full":2},"io":[{"backend_type":"client backend","context":"normal","object":"relation","writes":9,"writebacks":16,"fsyncs":0,"op_bytes":8192}],"bgwriter":{"maxwritten_clean":3,"buffers_clean":50},"temp_bytes":100}
{"t":"c","wal":{"wal_write":30,"wal_sync":14,"wal_write_time":7,"wal_sync_time":40,"wal_buffers_full":2},"io":[{"backend_type":"client backend","context":"normal","object":"relation","writes":9,"writebacks":16,"fsyncs":0,"op_bytes":8192}],"bgwriter":{"maxwritten_clean":3,"buffers_clean":50},"temp_bytes":100}
JSON
printf '%s\n' '@@source docker exec test' '@@t 1' '8:0 rbytes=1 wbytes=1000 rios=1 wios=1 dbytes=0 dios=0' '@@mem' '0' 'SwapTotal:  1000 kB' 'SwapFree:  900 kB' '@@end' \
  '@@t 2' '8:0 rbytes=1 wbytes=5000 rios=1 wios=1 dbytes=0 dios=0' '@@mem' 'SwapTotal:  1000 kB' 'SwapFree:  400 kB' '@@end' >"$dg/instrument/cgroup-io.txt"
printf '%s\n' '{"t":"a","waits":[{"application_name":"zf-serve","backend_type":"client backend","state":"active","wait_event_type":"IO","wait_event":"WALSync","backends":30}]}' \
  '{"t":"b","waits":[{"application_name":"zf-serve","backend_type":"client backend","state":"active","wait_event_type":null,"wait_event":null,"backends":2}]}' >"$dg/instrument/waits.ndjson"
summarize_io_diag "$dg"
json_of "$dg/io-diag.json" "assert r['latency']['slow_seconds']==2 and r['latency']['slow_second_indexes']==[1,3] and r['latency']['p99_ms']==300.0
p=r['pg']; assert p['wal_sync_ms_per_sync']['n']==1 and p['wal_sync_ms_per_sync']['max']==5.0 and p['wal_sync_total']==4
assert p['writeback_bytes_per_second']['max']==16*8192 and p['maxwritten_clean_total']==3 and p['wal_buffers_full_total']==2 and p['temp_bytes_total']==100
c=r['cgroup']; assert c['available'] and c['write_bytes_per_second']['max']==4000 and c['swap_kb']['SwapFree']['min']==400
assert r['waits']['top_backend_samples']=={'zf-serve|client backend|IO|WALSync':30}"
mkdir -p "$work/diag-none"; summarize_io_diag "$work/diag-none"
json_of "$work/diag-none/io-diag.json" "assert r['latency'] is None and not r['cgroup']['available'] and r['pg']['wal_sync_ms_per_sync'] is None"
# A data file keeps the side and SHOW values in its topology, and M8_DATA_SUFFIX keeps runs apart.
printf '{"pg_settings_side":{"side":"b","show":%s}}\n' "$show_b" >>"$work/topology"
(M8_DATA_SUFFIX=-ab-b1 write_data ingest 0 "$raw" "$work/topology" side-tool)
json_of "$out/ingest-ab-b1.json" "assert r['topology']['pg_settings_side']['side']=='b' and r['topology']['pg_settings_side']['show']['wal_buffers']=='64MB'"
# The driver, with a stub run.sh: ABAB order, one comparison JSON, side A restored after the last run and after a failure.
cat >"$work/bin/fakerun" <<'STUB'
#!/usr/bin/env bash
echo "$* side=${M8_PG_SETTINGS:-} diag=${M8_IO_DIAG:-} suffix=${M8_DATA_SUFFIX:-}" >>"$M8_TEST_RUNS"
[ "$1" != pg-settings ] || exit "${M8_TEST_RESTORE_RC:-0}"
n="${M8_DATA_SUFFIX: -1}"
[ "${M8_TEST_FAIL_ON:-}" != "$M8_PG_SETTINGS$n" ] || exit 3
p99=100; [ "$M8_PG_SETTINGS" = a ] || p99=50
[ "$n" = 1 ] || p99=$((p99 + 10))
printf '{"image_version":"0.233.0","exit_code":0,"topology":{"pg_settings_side":{"side":"%s","show":{"wal_buffers":"x"}}},"results":{"ingest.json":{"total":{"latency_from_schedule":{"p99_ms":%s,"max_ms":1},"achieved_rate":2000}},"noload/timers.json":{"lateness":{"overall":{"p99_s":2,"max_s":3}}},"io-diag.json":{"latency":{"slow_seconds":%s},"pg":{"wal_sync_ms_per_sync":{"p99":7,"max":9}}}}}\n' \
  "$M8_PG_SETTINGS" "$p99" "$([ "$M8_PG_SETTINGS" = a ] && echo 21 || echo 3)" >"$M8_DATA_DIR/$1$M8_DATA_SUFFIX.json"
STUB
chmod +x "$work/bin/fakerun"
ab_env() { # data dir, runs file; the rest is the command
  local dir="$1" runs="$2"; shift 2
  env M8_DATA_DIR="$dir" M8_TEST_RUNS="$runs" M8_AB_RUN_CMD="$work/bin/fakerun" BENCH_IMAGE_TAG=0.233.0 "$@"
}
mkdir -p "$work/ab-out" "$work/ab-out2"
: >"$work/ab-runs"
ab_env "$work/ab-out" "$work/ab-runs" bash "$here/ab-settings.sh" ingest 2 >/dev/null 2>&1
[ "$(sed 's/ diag.*//' "$work/ab-runs" | paste -sd, -)" = 'ingest side=a,ingest side=b,ingest side=a,ingest side=b,pg-settings a side=' ] ||
  { cat "$work/ab-runs" >&2; echo 'self-test FAILED: driver order' >&2; exit 1; }
grep -Fq 'diag=1 suffix=-ab-b2' "$work/ab-runs"
json_of "$work/ab-out/ab-settings-ingest.json" "assert r['order']=='ABAB' and r['complete'] and r['exit_code']==0 and r['image_version']=='0.233.0' and r['runs_per_side']==2 and len(r['runs'])==4
assert r['means']['a']['p99_ms']==105.0 and r['means']['b']['p99_ms']==55.0 and r['means']['a']['slow_seconds']==21 and r['means']['b']['slow_seconds']==3
assert r['means']['a']['wal_sync_ms_per_sync_max']==9 and r['runs'][1]['show']['wal_buffers']=='x' and r['runs'][1]['side']=='b' and r['topology']['sides']['b']
assert r['runs'][0]['data_file'].endswith('ingest-ab-a1.json')"
# A failing run stops the A/B, is recorded, and side A is still restored.
: >"$work/ab-runs2"
expect_failure 'run b1 failed (3)' ab_env "$work/ab-out2" "$work/ab-runs2" M8_TEST_FAIL_ON=b1 bash "$here/ab-settings.sh" timers 2
[ "$(sed 's/ diag.*//' "$work/ab-runs2" | paste -sd, -)" = 'timers side=a,timers side=b,pg-settings a side=' ]
json_of "$work/ab-out2/ab-settings-timers.json" "assert not r['complete'] and r['exit_code']==1 and [x['exit_code'] for x in r['runs']]==[0,3] and r['means']['a']['noload_p99_s']==2 and r['means']['b']['runs']==0"
# A failed restore fails the driver; a bad arm, a bad count and an old image are refused before any run.
expect_failure 'could not restore side A' ab_env "$work/ab-out" "$work/ab-runs" M8_TEST_RESTORE_RC=1 bash "$here/ab-settings.sh" ingest 1
expect_failure 'usage: ab-settings.sh' bash "$here/ab-settings.sh" burst 1
expect_failure 'runs-per-side must be an integer' bash "$here/ab-settings.sh" ingest 0
expect_failure 'below 0.233.0' env M8_DATA_DIR="$work/ab-out" M8_AB_RUN_CMD="$work/bin/fakerun" BENCH_IMAGE_TAG=0.232.9 bash "$here/ab-settings.sh" ingest 1
expect_failure 'unknown settings side' bash "$here/run.sh" pg-settings c
bash "$here/run.sh" --help | grep -Fq pg-settings

echo 'm8 self-test: parsing, registration, cpusets, disk/memory guards, metadata and prebuilt render passed'
