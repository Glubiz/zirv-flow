#!/usr/bin/env bash
# No-op throughput and OQ-8: a busy workspace ahead of a quiet job, uncapped and capped.
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"
host_pg_setup
cap="${M8_JOBS_CAP:-4}"
busy="${M8_JOBS_BUSY:-100000}"
lead="${M8_JOBS_LEAD_SECONDS:-10}"
timeout="${M8_JOBS_TIMEOUT_SECONDS:-600}"
tick="${M8_JOBS_TICK_SECONDS:-0.2}"
[ -z "${M8_SMOKE:-}" ] || busy=3000
for name in busy lead timeout; do positive_integer "$name" "${!name}"; done
[ "$cap" = unset ] || positive_integer M8_JOBS_CAP "$cap"
[[ "$tick" =~ ^[0-9]+([.][0-9]+)?$ && "$tick" =~ [1-9] ]] || die 'M8_JOBS_TICK_SECONDS must be positive'
require_job_sampler
cid="$(compose ps -q zirv-flow-worker)"
[ -n "$cid" ] && [[ "$cid" != *$'\n'* ]] || die 'expected the single M8 worker with its published metrics port'
concurrency="$(docker inspect --format '{{json .Config.Env}}' "$cid" | python3 -I -c '
import json, sys
env = dict(item.split("=", 1) for item in json.load(sys.stdin))
print(env["ZIRV_FLOW_WORKER__CONCURRENCY"])
')"
positive_integer worker.concurrency "$concurrency"
export BENCH_WORKER_CONCURRENCY="$concurrency"
sampler_pid=''; restore_worker=''; probe=''
cleanup() {
  if [ -n "$sampler_pid" ]; then kill "$sampler_pid" 2>/dev/null || true; wait "$sampler_pid" 2>/dev/null || true; fi
  if [ -n "$probe" ]; then
    host_pg -c 'DROP TRIGGER IF EXISTS m8_quiet_claim ON jobs; DROP FUNCTION IF EXISTS m8bench.record_quiet_claim()' || log 'failed to remove quiet-claim probe'
  fi
  if [ -n "$restore_worker" ]; then compose up -d --no-build --no-deps --force-recreate zirv-flow-worker; fi
}
trap cleanup EXIT
caps=(unset)
[ "$cap" = unset ] || caps+=("$cap")
for setting in "${caps[@]}"; do
  dest="$raw/cap-$setting"
  mkdir -p "$dest"
  python3 -I - "$dest/worker.yaml" "$setting" <<'PY'
import json, pathlib, sys
cap = None if sys.argv[2] == 'unset' else sys.argv[2]
pathlib.Path(sys.argv[1]).write_text(json.dumps({'services': {'zirv-flow-worker': {
    'environment': {'ZIRV_FLOW_WORKER__MAX_RUNNING_PER_WORKSPACE': cap}}}}) + '\n')
PY
  restore_worker=1
  compose -f "$dest/worker.yaml" up -d --no-build --no-deps --force-recreate zirv-flow-worker
  cid="$(compose ps -q zirv-flow-worker)"
  [ -n "$cid" ] && [[ "$cid" != *$'\n'* ]] || die 'expected the single M8 worker with its published metrics port'
  docker inspect --format '{{json .Config.Env}}' "$cid" | python3 -I -c '
import json, sys
env = dict(item.split("=", 1) for item in json.load(sys.stdin))
cap = env.get("ZIRV_FLOW_WORKER__MAX_RUNNING_PER_WORKSPACE")
expected = None if sys.argv[2] == "unset" else sys.argv[2]
if cap != expected:
    raise SystemExit("worker cap differs from requested scenario")
json.dump(dict(worker_containers=1, concurrency_per_worker=int(env["ZIRV_FLOW_WORKER__CONCURRENCY"]),
               max_running_per_workspace=int(cap) if cap else None), open(sys.argv[1], "w"), indent=2)
' "$dest/worker.json" "$setting"
  wait_worker_metrics
  trace="$(python3 -I -c 'import uuid; print(uuid.uuid4())')"
  probe=1
  host_pg -v trace="$trace" -v busy="$busy" -v lead="$lead" -f "$m8_dir/jobs.sql" >"$dest/enqueue.log"
  start_job_sampler "$dest"
  deadline=$((SECONDS + lead + timeout))
  while :; do
    host_pg -At -v trace="$trace" -f - >>"$dest/samples.ndjson" <<'SQL'
SELECT json_build_object('t', extract(epoch FROM clock_timestamp()),
  'ready', (SELECT count(*) FROM jobs WHERE state = 'ready' AND kind = 'maintenance.noop' AND trace_id = :'trace'::uuid),
  'running', (SELECT count(*) FROM jobs WHERE state = 'running' AND workspace_id IN (m8bench.ws(1), m8bench.ws(2)) AND trace_id = :'trace'::uuid),
  'dead', (SELECT count(*) FROM jobs WHERE state = 'dead' AND trace_id = :'trace'::uuid),
  'busy_running', (SELECT count(*) FROM jobs WHERE state = 'running' AND workspace_id = m8bench.ws(1)));
SQL
    state="$(python3 -I - "$dest/samples.ndjson" "$setting" <<'PY'
import json, pathlib, sys
r = json.loads(pathlib.Path(sys.argv[1]).read_text().splitlines()[-1])
if r['dead']:
    print('dead')
elif sys.argv[2] != 'unset' and r['busy_running'] > int(sys.argv[2]):
    print('cap_exceeded')
else:
    print('done' if r['ready'] + r['running'] == 0 else 'waiting')
PY
)"
    [ "$state" = waiting ] || break
    if [ "$SECONDS" -ge "$deadline" ]; then state=timeout; break; fi
    sleep "$tick"
  done
  stop_job_sampler "$dest"
  host_pg -At -v trace="$trace" -f - >"$dest/probe.json.tmp" <<'SQL'
SELECT to_jsonb(p) || jsonb_build_object('ready_epoch', extract(epoch FROM ready_at))
FROM m8bench.jobs_probe p WHERE trace_id = :'trace'::uuid;
SQL
  mv "$dest/probe.json.tmp" "$dest/probe.json"
  scrape_metrics http://127.0.0.1:9100/metrics zirv_flow_jobs_total "$dest/metrics"
  python3 -I - "$dest" "$tick" "$state" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
probe = json.loads((path / 'probe.json').read_text())
samples = [json.loads(line) for line in (path / 'samples.ndjson').read_text().splitlines()]
seconds = samples[-1]['t'] - probe['ready_epoch']
completed = probe['busy'] + 1 - sum(samples[-1][k] for k in ('ready', 'running', 'dead'))
rate = completed / seconds if seconds > 0 else None
report = dict(jobs=probe['busy'] + 1, completed=completed, elapsed_s=seconds, completed_per_s=rate,
    target_per_s=1000, meets_target=rate is not None and rate >= 1000, quiet_claim_lag_s=probe['quiet_claim_lag_s'],
    busy_max_running_sampled=max(s['busy_running'] for s in samples), sample_sleep_s=float(sys.argv[2]),
    measurement='enqueue-to-last-empty-poll; includes polling delay; running peak is sampled',
    worker=json.loads((path / 'worker.json').read_text()), samples=samples, outcome=sys.argv[3])
(path / 'jobs.json').write_text(json.dumps(report, indent=2) + '\n')
if sys.argv[3] != 'done' or seconds <= 0 or probe['quiet_claim_lag_s'] is None:
    raise SystemExit('incomplete or invalid jobs scenario: ' + sys.argv[3])
PY
  host_pg -c 'DROP TRIGGER m8_quiet_claim ON jobs; DROP FUNCTION m8bench.record_quiet_claim()'
  probe=''
done
compose up -d --no-build --no-deps --force-recreate zirv-flow-worker
restore_worker=''
