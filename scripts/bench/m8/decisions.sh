#!/usr/bin/env bash
# Decision-log overhead A/B (#75) on the M8 emulation, in container networking. Adapted from scripts/bench/ab.sh.
# Each side drains the same pre-loaded backlog through a serve, worker and scheduler container (the M8 cpusets and
# memory caps) against a fresh PostgreSQL container (the M8 settings); only the binary differs:
#   $M8_STATE/ab-bin/zirv-flow-on   a plain release build
#   $M8_STATE/ab-bin/zirv-flow-off  the same source built with ZIRV_FLOW_BENCH_BUILD=1 --features zirv-flow-store/bench-no-decisions
# Build them with scripts/bench/m8/build-ab.sh. The M8 stack is stopped for the run (cpusets and memory are shared)
# and started again at the end; its volumes are never touched. This script's own containers and volumes are named
# <project>-ab-* and are removed after each side.
# Usage: decisions.sh <raw dir>   Env: M8_AB_RUNS (2 per side), M8_AB_ITEMS (600000), M8_AB_RATE (5000)
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"
host_pg_setup
runs="${M8_AB_RUNS:-2}"; items="${M8_AB_ITEMS:-600000}"; rate="${M8_AB_RATE:-5000}"
for name in runs items rate; do positive_integer "$name" "${!name}"; done
[ -z "${M8_SMOKE:-}" ] || { runs=1; items=5000; rate=500; }
bin="$M8_STATE/ab-bin"
loadgen="${LOADGEN:-$root/target/release/loadgen}"
[ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
for side in on off; do [ -s "$bin/zirv-flow-$side" ] || die "missing $bin/zirv-flow-$side; run scripts/bench/m8/build-ab.sh"; done
base="${M4_PROJECT}-ab"
net="$base-net"; pgc="$base-postgres"; servec="$base-serve"; workerc="$base-worker"; schedc="$base-scheduler"
pgvol="$base-pgdata"; keyvol="$base-keys"
appimage="${M8_AB_APP_IMAGE:-debian:bookworm-slim}"
pw='ab-bench-pass-12345'
scratch="$M8_STATE/ab-scratch"; mkdir -p "$scratch" "$raw"
win() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
d() { MSYS_NO_PATHCONV=1 docker "$@"; }
q() { d exec -i "$pgc" psql -X -q -U zirv -d zirv_flow_ab -v ON_ERROR_STOP=1 "$@"; }

teardown() {
  d rm -f "$workerc" "$schedc" "$servec" "$pgc" >/dev/null 2>&1 || true
  d volume rm -f "$pgvol" >/dev/null 2>&1 || true
}
restored=''
finish() {
  local rc=$?
  trap - EXIT
  teardown
  d volume rm -f "$keyvol" >/dev/null 2>&1 || true
  d network rm "$net" >/dev/null 2>&1 || true
  if [ -z "$restored" ]; then
    log 'restarting the M8 stack'
    compose up -d --no-build --wait --wait-timeout 600 postgres zirv-flow-key zirv-flow || rc=1
    compose up -d --no-build --wait --wait-timeout 300 zirv-flow-worker zirv-flow-scheduler || rc=1
  fi
  exit "$rc"
}
trap finish EXIT

compose stop -t 600
teardown
d network create "$net" >/dev/null
d volume create "$keyvol" >/dev/null
d run --rm -v "$keyvol:/keys" -v "$(win "$bin"):/bin-ab:ro" "$appimage" \
  /bin-ab/zirv-flow-on admin generate-master-key --out /keys/master.key >/dev/null

appenv=(-e ZIRV_FLOW_DATABASE__URL=postgres://zirv:zirv-m8-bench@postgres:5432/zirv_flow_ab
  -e ZIRV_FLOW_SECRETS__MASTER_KEY_FILE=/keys/master.key -e ZIRV_FLOW_SERVER__COOKIE_SECURE=false
  -e ZIRV_FLOW_LOG__FILTER=warn -e ZIRV_FLOW_INGEST__DRAIN_SHARDS=8
  -e ZIRV_FLOW_INGEST__RATE_PER_KEY=10000 -e ZIRV_FLOW_INGEST__BURST_PER_KEY=20000
  -e ZIRV_FLOW_WORKER__CONCURRENCY=4 -e ZIRV_FLOW_SERVER__BIND=0.0.0.0:8080)

start_app() { # name cpuset memory side role [extra docker args...]
  local name="$1" cpus="$2" mem="$3" side="$4" role="$5"; shift 5
  d run -d --name "$name" --network "$net" --cpuset-cpus "$cpus" -m "$mem" \
    -v "$(win "$bin/zirv-flow-$side"):/usr/local/bin/zirv-flow:ro" -v "$keyvol:/keys:ro" "${appenv[@]}" "$@" \
    "$appimage" /usr/local/bin/zirv-flow "$role" >/dev/null
}

run_side() { # side n
  local side="$1" n="$2" out="$raw/$1$2.json" t0 t1 backlog decisions events
  log "A/B side $side run $n"
  teardown
  d volume create "$pgvol" >/dev/null
  d run -d --name "$pgc" --network "$net" --network-alias postgres --cpuset-cpus "$M8_PG_CPUSET" -m "$M8_PG_MEMORY" \
    --shm-size "$M8_PG_SHM" -e POSTGRES_USER=zirv -e POSTGRES_PASSWORD=zirv-m8-bench -e POSTGRES_DB=zirv_flow_ab \
    -v "$pgvol:/var/lib/postgresql/data" "$M8_PG_IMAGE" postgres -c "shared_buffers=${M8_SHARED_BUFFERS:-8GB}" \
    -c effective_cache_size=16GB -c maintenance_work_mem=1GB -c "max_wal_size=${M8_MAX_WAL_SIZE:-32GB}" \
    -c wal_compression=lz4 -c checkpoint_timeout=15min -c "max_connections=${M8_PG_MAX_CONNECTIONS:-400}" >/dev/null
  until d exec "$pgc" pg_isready -U zirv -d zirv_flow_ab >/dev/null 2>&1; do sleep 1; done
  sleep 3
  start_app "$servec" "$M8_SERVE_CPUSET" "$M8_SERVE_MEMORY" "$side" serve -p 127.0.0.1:18080:8080 \
    -e ZIRV_FLOW_DATABASE__MAX_CONNECTIONS=32 -e ZIRV_FLOW_DATABASE__MIGRATE_ON_START=true
  local deadline=$((SECONDS + 300))
  until curl -fsS --connect-timeout 2 --max-time 5 http://127.0.0.1:18080/readyz >/dev/null 2>&1; do
    [ "$SECONDS" -lt "$deadline" ] || die 'A/B serve did not become ready'
    sleep 1
  done
  printf '%s\n' "$pw" | d exec -i "$servec" /usr/local/bin/zirv-flow admin create-owner --email bench@example.com >/dev/null
  local headers csrf session ws
  headers="$(mktemp)"
  csrf="$(jq -n --arg p "$pw" '{email:"bench@example.com",password:$p}' \
    | curl -fsS -D "$headers" -X POST 127.0.0.1:18080/api/v1/session -H 'content-type: application/json' --data-binary @- | jq -r .csrf_token)"
  session="$(tr -d '\r' <"$headers" | sed -nE 's/^[Ss]et-[Cc]ookie: (zf_session=[^;]+).*/\1/p' | head -n1)"
  rm -f "$headers"
  ws="$(curl -fsS -X POST 127.0.0.1:18080/api/v1/workspaces -H "cookie: $session" -H "x-csrf-token: $csrf" \
    -H 'content-type: application/json' -d '{"name":"ab"}' | jq -r .id)"
  (umask 077; curl -fsS -X POST "127.0.0.1:18080/api/v1/workspaces/$ws/api-keys" -H "cookie: $session" -H "x-csrf-token: $csrf" \
    -H 'content-type: application/json' -d '{"name":"loadgen","scopes":["ingest"]}' | jq -r .key >"$scratch/ab-key")
  # The backlog: no worker is running, so the inbox only fills.
  "$loadgen" --url http://127.0.0.1:18080 --key-file "$scratch/ab-key" --mode track --rate "$rate" \
    --duration $((items / rate)) --people 100000 --run-id "ab-$side-$n" --out "$scratch/ab-load.json" >/dev/null 2>&1
  backlog="$(q -At -c 'SELECT count(*) FROM ingest_inbox')"
  start_app "$schedc" "$M8_WORKER_CPUSET" "$M8_SCHEDULER_MEMORY" "$side" scheduler
  start_app "$workerc" "$M8_WORKER_CPUSET" "$M8_WORKER_MEMORY" "$side" worker -e ZIRV_FLOW_OPS__BIND=0.0.0.0:9100 -p 127.0.0.1:19100:9100
  t0="$(date +%s.%N)"
  until [ "$(q -At -c 'SELECT count(*) FROM ingest_inbox')" = 0 ]; do sleep 0.5; done
  t1="$(date +%s.%N)"
  curl -fsS --connect-timeout 5 --max-time 10 http://127.0.0.1:19100/metrics >"$raw/$side$n.worker-metrics.prom"
  decisions="$(q -At -c "SELECT count(*) FROM decision_records WHERE action <> 'job.attempt'")"
  events="$(q -At -c 'SELECT count(*) FROM events')"
  jq -nc --arg side "$side" --argjson n "$n" --argjson backlog "$backlog" --argjson t0 "$t0" --argjson t1 "$t1" \
    --argjson decisions "$decisions" --argjson events "$events" \
    '{side:$side, run:$n, backlog_items:$backlog, drain_seconds:($t1-$t0), applied_items_per_s:($events/($t1-$t0)),
      events_stored:$events, decision_records:$decisions}' >"$out"
  teardown
}

for ((n = 1; n <= runs; n++)); do
  run_side on "$n"
  run_side off "$n"
done

# Topology for write_data, from the A/B containers' configuration (they are gone by now, so it is recorded from the requested caps).
topo="$raw/topology.ndjson"
: >"$topo"
for row in "postgres:$M8_PG_CPUSET:$M8_PG_MEMORY:$M8_PG_IMAGE" "zirv-flow:$M8_SERVE_CPUSET:$M8_SERVE_MEMORY:$appimage" \
  "zirv-flow-worker:$M8_WORKER_CPUSET:$M8_WORKER_MEMORY:$appimage" "zirv-flow-scheduler:$M8_WORKER_CPUSET:$M8_SCHEDULER_MEMORY:$appimage"; do
  IFS=: read -r svc cpus mem img <<<"$row"
  jq -nc --arg s "$svc" --arg c "$cpus" --arg i "$img" --argjson m "$(mem_bytes "$mem")" \
    '{service:$s, image:$i, image_id:"", cpuset:$c, memory_bytes:$m, shm_bytes:0}' >>"$topo"
done
jq -nc --arg sb "${M8_SHARED_BUFFERS:-8GB}" --arg wal "${M8_MAX_WAL_SIZE:-32GB}" \
  '{postgres_settings:{shared_buffers:$sb, effective_cache_size:"16GB", maintenance_work_mem:"1GB", max_wal_size:$wal,
    checkpoint_timeout:"15min", wal_compression:"lz4", max_connections:"400"}}' >>"$topo"
python3 -I - "$raw" "$items" <<'PY'
import json, pathlib, re, sys
raw = pathlib.Path(sys.argv[1])
def p95(prom):
    buckets, count = [], 0.0
    for line in prom.read_text().splitlines():
        if 'zirv_flow_job_duration_seconds' in line and 'kind="ingest.drain"' in line:
            m = re.search(r'le="([^"]+)"\} (\S+)', line)
            if '_bucket' in line and m:
                buckets.append((float('inf') if m[1] == '+Inf' else float(m[1]), float(m[2])))
    buckets.sort()
    if not buckets or buckets[-1][1] == 0:
        return None
    target, prev_le, prev_c = 0.95 * buckets[-1][1], 0.0, 0.0
    for le, c in buckets:
        if c >= target:
            if le == float('inf'):
                return prev_le * 1000
            return (prev_le + (le - prev_le) * (target - prev_c) / max(c - prev_c, 1e-9)) * 1000
        prev_le, prev_c = le, c
runs = {'on': [], 'off': []}
for f in sorted(raw.glob('o[nf]*[0-9].json')):
    r = json.loads(f.read_text())
    r['drain_job_p95_ms'] = p95(raw / (f.stem + '.worker-metrics.prom'))
    runs[r['side']].append(r)
mean = lambda xs: sum(xs) / len(xs) if xs else None
on_t = mean([r['drain_seconds'] for r in runs['on']]); off_t = mean([r['drain_seconds'] for r in runs['off']])
on_p = mean([r['drain_job_p95_ms'] for r in runs['on'] if r['drain_job_p95_ms'] is not None])
off_p = mean([r['drain_job_p95_ms'] for r in runs['off'] if r['drain_job_p95_ms'] is not None])
summary = dict(runs=runs, mean_drain_seconds=dict(on=on_t, off=off_t),
    throughput_loss=(on_t - off_t) / on_t if on_t and off_t else None, target_throughput_loss=0.10,
    mean_drain_job_p95_ms=dict(on=on_p, off=off_p),
    added_p95_ms=on_p - off_p if on_p is not None and off_p is not None else None, target_added_p95_ms=5,
    measurement='added p95 is the difference of the ingest.drain job duration p95 (histogram interpolation, worker metrics) with and without decision writes; one drain job is one transaction of up to ingest.drain_batch_size rows, so this is a proxy for the added transaction time',
    items=int(sys.argv[2]))
(raw / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
PY
restored=''
