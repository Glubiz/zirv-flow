#!/usr/bin/env bash
# Explorer latency and ingest interference at the retained scale actually present.
# shellcheck source=../m4/common.sh
. "$(dirname "${BASH_SOURCE[0]}")/../m4/common.sh"
# shellcheck source=common.sh
. "$root/scripts/bench/m8/common.sh"

explorer_main() {
raw="$1/explorer"; pool="$2"
host_pg_setup
seconds="$M8_SECONDS"
drain="${M8_DRAIN_SECONDS:-60}"
clients=(1 4 16)
# Reruns: M8_EXPLORER_CLIENTS="16" and M8_EXPLORER_SETTINGS="default 15000" pick the scenarios (defaults: all).
[ -z "${M8_EXPLORER_CLIENTS:-}" ] || read -r -a clients <<<"$M8_EXPLORER_CLIENTS"
settings=(default 15000)
[ -z "${M8_EXPLORER_SETTINGS:-}" ] || read -r -a settings <<<"$M8_EXPLORER_SETTINGS"
# M8_EXPLORER_SHAPES="export" or "interactive" (or shape names, comma separated) runs one family of shapes; default all seven.
shapes="${M8_EXPLORER_SHAPES:-}"
[[ -z "$shapes" || "$shapes" =~ ^[a-z_,]+$ ]] || die 'M8_EXPLORER_SHAPES must be comma separated shape names, interactive or export'
# M8_EXPLORER_CONTAINER=1 runs the explorer loadgen in a Linux container on the bench network (as the debugger's container
# mode does), so the host network path is out of the explorer measurement. Ingest load stays on the host.
container="${M8_EXPLORER_CONTAINER:-}"
linux_loadgen="$M8_STATE/send-tools/linux/loadgen"
cname="${M4_PROJECT}-explorer-loadgen"
explorer_image="${M8_EXPLORER_IMAGE:-postgres:16.15}"
win() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
if [ -n "${M8_SMOKE:-}" ]; then seconds=5; clients=(1); fi
for name in seconds drain; do positive_integer "$name" "${!name}"; done
loadgen="${LOADGEN:-$root/target/release/loadgen}"
if [ -n "$container" ]; then
  [ -s "$linux_loadgen" ] || die "no Linux loadgen at $linux_loadgen (build it in a rust:1.98-bookworm container, as for sendbench)"
else
  [ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
fi
mkdir -p "$raw"
ws="$(state_get M4_WS)"
[ -n "$ws" ] || die 'no workspace; seed people first'
login || die 'cannot sign in to create the explorer read key'
if [ ! -s "$M4_STATE/explorer.key" ]; then
  (umask 077; api_json POST "/workspaces/$ws/api-keys" '{"name":"m8-explorer","scopes":["read"]}' \
    | jq -er .key >"$M4_STATE/explorer.key")
fi
host_pg -At -v ws="$ws" -f - >"$raw/scale.json.tmp" <<'SQL'
SELECT json_build_object(
  'retained_rows', count(*),
  'workspace_retained_rows', count(*) FILTER (WHERE workspace_id = :'ws'::uuid),
  'workspace_window_rows', count(*) FILTER (WHERE workspace_id = :'ws'::uuid
      AND occurred_at >= (SELECT seed_at - interval '90 days' FROM m8bench.config)
      AND occurred_at < (SELECT seed_at FROM m8bench.config)),
  'seed_committed_rows', (SELECT sum(last_n) FROM m8bench.progress WHERE kind = 'events'),
  'from', (SELECT to_char(seed_at AT TIME ZONE 'UTC' - interval '90 days', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') FROM m8bench.config),
  'to', (SELECT to_char(seed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') FROM m8bench.config),
  'person_id', md5(:'ws' || '/p/1')::uuid,
  'person_exists', EXISTS (SELECT 1 FROM people WHERE workspace_id = :'ws'::uuid AND id = md5(:'ws' || '/p/1')::uuid),
  'measurement', 'exact retained counts before load; not the requested seed target')
FROM events;
SQL
mv "$raw/scale.json.tmp" "$raw/scale.json"
read -r from to person < <(python3 -I - "$raw/scale.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
if not r['person_exists'] or not r['workspace_window_rows']:
    raise SystemExit('seed people and events before running explorer')
print(r['from'], r['to'], r['person_id'])
PY
)
[ -n "$from" ] && [ -n "$to" ] && [ -n "$person" ] || die 'invalid seed window'

load_pid=''; explorer_pid=''; restore_serve=''
cleanup() {
  local rc=$? pid
  trap - EXIT
  for pid in "$load_pid" "$explorer_pid"; do
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  if [ -n "$container" ]; then docker rm -f "$cname" >/dev/null 2>&1 || true; fi
  if [ -n "$restore_serve" ]; then
    if ! compose up -d --no-build --no-deps --force-recreate zirv-flow; then
      log 'failed to restore base serve configuration'; rc=1
    fi
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
metrics='zirv_flow_explorer_query_seconds,zirv_flow_explorer_timeouts_total'
for setting in "${settings[@]}"; do
  dest="$raw/timeout-$setting"
  mkdir -p "$dest"
  python3 -I - "$dest/serve.yaml" "$setting" <<'PY'
import json, pathlib, sys
value = None if sys.argv[2] == 'default' else sys.argv[2]
pathlib.Path(sys.argv[1]).write_text(json.dumps({'services': {'zirv-flow': {'environment': {
    'ZIRV_FLOW_EXPLORER__INTERACTIVE_TIMEOUT_MS': value}}}}) + '\n')
PY
  restore_serve=1
  compose -f "$dest/serve.yaml" up -d --no-build --no-deps --force-recreate zirv-flow
  cid="$(compose ps -q zirv-flow)"
  [ -n "$cid" ] && [[ "$cid" != *$'\n'* ]] || die 'expected a single M8 serve container'
  docker inspect --format '{{json .Config.Env}}' "$cid" | python3 -I -c '
import json, sys
# A null override can list the bare key without a value; that variable is unset in the process.
env = dict(item.split("=", 1) for item in json.load(sys.stdin) if "=" in item)
value = env.get("ZIRV_FLOW_EXPLORER__INTERACTIVE_TIMEOUT_MS")
expected = None if sys.argv[2] == "default" else sys.argv[2]
if value != expected:
    raise SystemExit("serve timeout differs from requested scenario")
json.dump(dict(interactive_timeout_ms=int(value) if value else 5000,
               configuration="shipped default" if value is None else "environment override",
               export_timeout_ms=60000), open(sys.argv[1], "w"), indent=2)
' "$dest/config.json" "$setting"
  deadline=$((SECONDS + 60))
  until curl -fsS --connect-timeout 2 --max-time 5 http://127.0.0.1:8080/readyz >/dev/null 2>&1; do
    [ "$SECONDS" -lt "$deadline" ] || die 'serve did not become ready'
    sleep 1
  done
  wait_inbox "$drain"
  (track_load "$seconds" "$pool" "$dest/ingest-alone.json") >"$dest/ingest-alone.log" 2>&1
  for count in "${clients[@]}"; do
    wait_inbox "$drain"
    scenario="$dest/clients-$count"
    mkdir -p "$scenario"
    scrape_metrics http://127.0.0.1:8080/metrics "$metrics" "$scenario/metrics-before"
    track_load "$seconds" "$pool" "$scenario/ingest.json" >"$scenario/ingest.log" 2>&1 &
    load_pid=$!
    explorer_args=(--workspace "$ws" --person-id "$person" --to "$to" --clients "$count" --duration "$seconds")
    [ -z "$shapes" ] || explorer_args+=(--shapes "$shapes")
    if [ -n "$container" ]; then
      docker rm -f "$cname" >/dev/null 2>&1 || true
      MSYS_NO_PATHCONV=1 docker run --rm --name "$cname" --network "${M4_PROJECT}_default" \
        -v "$(win "$M8_STATE/send-tools/linux"):/opt/lg:ro" -v "$(win "$M4_STATE"):/auth:ro" -v "$(win "$scenario"):/raw" \
        "$explorer_image" /opt/lg/loadgen explorer --url http://zirv-flow:8080 --key-file /auth/explorer.key \
        "${explorer_args[@]}" --out /raw/queries.json >"$scenario/explorer.log" 2>&1 &
    else
      "$loadgen" explorer --url http://127.0.0.1:8080 --key-file "$M4_STATE/explorer.key" \
        "${explorer_args[@]}" --out "$scenario/queries.json" >"$scenario/explorer.log" 2>&1 &
    fi
    explorer_pid=$!
    wait "$load_pid"; load_pid=''
    wait "$explorer_pid"; explorer_pid=''
    scrape_metrics http://127.0.0.1:8080/metrics "$metrics" "$scenario/metrics-after"
    python3 -I - "$dest" "$scenario" "$raw/scale.json" "$M8_RATE" "${container:+container}" "${shapes:-all}" <<'PY'
import json, pathlib, sys
base, path = map(pathlib.Path, sys.argv[1:3])
read = lambda p: json.loads(p.read_text())
solo = read(base / 'ingest-alone.json')['total']
loaded = read(path / 'ingest.json')['total']
a, b = (r['latency_from_schedule']['p99_ms'] for r in (solo, loaded))
report = dict(scale=read(pathlib.Path(sys.argv[3])), configuration=read(base / 'config.json'),
    queries=read(path / 'queries.json'), ingest_rate=int(sys.argv[4]),
    explorer_loadgen_runs_in=sys.argv[5] or 'host', shapes_selected=sys.argv[6],
    ingest_p99_alone_ms=a, ingest_p99_with_explorer_ms=b,
    ingest_p99_delta_ms=b-a, ingest_p99_ratio=b/a if a else None,
    ingest_achieved_rate_alone=solo['achieved_rate'], ingest_achieved_rate_with_explorer=loaded['achieved_rate'],
    measurement='ingest and explorer launched together for the same duration; explorer drains in-flight requests afterwards; ingest p99 is from schedule; EXPLAIN and retained counts run outside this window')
(path / 'comparison.json').write_text(json.dumps(report, indent=2) + '\n')
PY
  done
done
wait_inbox "$drain"
# The same SQL shapes apply to both budgets. Collect once, after the timed workload.
for shape in count_name count_property count_person breakdown_low breakdown_high list export; do
  host_pg -At -v shape="$shape" -v ws="$ws" -v from="$from" -v to="$to" -v person="$person" \
    -f "$m8_dir/explorer.sql" >"$raw/$shape.plan"
  python3 -I - "$raw/$shape.plan" "$raw/$shape.json" <<'PY'
import json, pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text().strip()
plans = []
decoder = json.JSONDecoder()
while text:
    plan, end = decoder.raw_decode(text)
    plans.append(plan)
    text = text[end:].lstrip()
if not plans:
    raise SystemExit('EXPLAIN returned no plans')
pathlib.Path(sys.argv[2]).write_text(json.dumps({'plans': plans}, indent=2) + '\n')
PY
done
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then explorer_main "$@"; fi
