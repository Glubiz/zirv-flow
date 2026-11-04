#!/usr/bin/env bash
# Decision-log overhead A/B (#75) on the M8 emulation, in container networking. Adapted from scripts/bench/ab.sh.
# Each side drains the same pre-loaded backlog through a serve, worker and scheduler container (the M8 cpusets and
# memory caps) against a fresh PostgreSQL container (the M8 settings); only the binary differs:
#   $M8_STATE/ab-bin/zirv-flow-on   a plain release build
#   $M8_STATE/ab-bin/zirv-flow-off  the same source built with ZIRV_FLOW_BENCH_BUILD=1 --features zirv-flow-store/bench-no-decisions
# Build them with scripts/bench/m8/build-ab.sh. The M8 stack is stopped for the run (cpusets and memory are shared)
# and started again at the end; its volumes are never touched. This script's own containers and volumes are named
# <project>-ab-* and are removed after each side.
# Per run (<side><n>.instrument/): docker stats of the four containers by name, pg_stat_statements and pg_stat_wal (WAL
# bytes, records, full-page images) before and after the drain, and the wait-event sampler. The report's added p95 is the
# worker's zirv_flow_decision_append_seconds histogram (the "off" build records none); the throughput loss is paired per round.
# Rounds are interleaved and alternate which side goes first.
# Usage: decisions.sh <raw dir>   Env: M8_DECISIONS_RUNS (3 per side), M8_AB_ITEMS (600000), M8_AB_RATE (5000)
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"
host_pg_setup
runs="${M8_DECISIONS_RUNS:-${M8_AB_RUNS:-3}}"; items="${M8_AB_ITEMS:-600000}"; rate="${M8_AB_RATE:-5000}"
for name in runs items rate; do positive_integer "$name" "${!name}"; done
[ -z "${M8_SMOKE:-}" ] || { runs=1; items=5000; rate=500; }
bin="$M8_STATE/ab-bin"
loadgen="${LOADGEN:-$root/target/release/loadgen}"
[ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
for side in on off; do [ -s "$bin/zirv-flow-$side" ] || die "missing $bin/zirv-flow-$side; run scripts/bench/m8/build-ab.sh"; done
[ -s "$bin/source-rev" ] || die "missing $bin/source-rev; build with scripts/bench/m8/build-ab.sh"
base="${M4_PROJECT}-ab"
net="$base-net"; pgc="$base-postgres"; servec="$base-serve"; workerc="$base-worker"; schedc="$base-scheduler"
pgvol="$base-pgdata"; keyvol="$base-keys"
appimage="${M8_AB_APP_IMAGE:-debian:bookworm-slim}"
pw='ab-bench-pass-12345'
scratch="$M8_STATE/ab-scratch"; mkdir -p "$scratch" "$raw"
win() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
d() { MSYS_NO_PATHCONV=1 docker "$@"; }
q() { d exec -i "$pgc" psql -X -q -U zirv -d zirv_flow_ab -v ON_ERROR_STOP=1 "$@"; }

# The A/B database is its own container: samplers live in it, and the M8 stack's database may be stopped.
terminate_pg_samplers() {
  q -At -c "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name LIKE 'm8-sampler%' AND pid <> pg_backend_pid()" >/dev/null 2>&1 || true
}
idir=''
stop_run_samplers() {
  stop_docker_stats
  stop_waits_sampler
}
teardown() {
  d rm -f "$workerc" "$schedc" "$servec" "$pgc" >/dev/null 2>&1 || true
  d volume rm -f "$pgvol" >/dev/null 2>&1 || true
}
restored=''
finish() {
  local rc=$?
  trap - EXIT
  stop_run_samplers
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
trap 'exit 130' INT
trap 'exit 143' TERM

assert_no_samplers
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

start_ab_samplers() {
  local names=("$pgc" "$servec" "$workerc" "$schedc")
  (
    while :; do
      docker_stats_sample "${names[@]}"
      sleep "${M8_DOCKER_STATS_TICK_S:-1}"
    done
  ) >"$idir/docker-stats.ndjson" 2>/dev/null &
  stats_pid=$!
  register_sampler docker-stats "$stats_pid"
  (exec docker exec -i -e PGAPPNAME=m8-sampler-waits "$pgc" psql -X -q -U zirv -d zirv_flow_ab -At -f - <"$m8_dir/pg-waits.sql") \
    >"$idir/waits.ndjson" 2>"$idir/waits.err" &
  waits_pid=$!
  register_sampler waits "$waits_pid"
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
    -c wal_compression=lz4 -c checkpoint_timeout=15min -c "max_connections=${M8_PG_MAX_CONNECTIONS:-400}" \
    -c shared_preload_libraries=pg_stat_statements >/dev/null
  until d exec "$pgc" pg_isready -U zirv -d zirv_flow_ab >/dev/null 2>&1; do sleep 1; done
  sleep 3
  q -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
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
  # The measured window starts here: the statement counters are reset, the WAL counters snapshotted, the samplers started.
  idir="$raw/$side$n.instrument"; mkdir -p "$idir"
  q -At -c 'SELECT pg_stat_statements_reset()' >/dev/null
  q -At -f - <"$m8_dir/pg-stats.sql" >"$idir/pg-stats-before.json"
  start_ab_samplers
  start_app "$schedc" "$M8_WORKER_CPUSET" "$M8_SCHEDULER_MEMORY" "$side" scheduler
  start_app "$workerc" "$M8_WORKER_CPUSET" "$M8_WORKER_MEMORY" "$side" worker -e ZIRV_FLOW_OPS__BIND=0.0.0.0:9100 -p 127.0.0.1:19100:9100
  t0="$(date +%s.%N)"
  until [ "$(q -At -c 'SELECT count(*) FROM ingest_inbox')" = 0 ]; do sleep 0.5; done
  t1="$(date +%s.%N)"
  stop_run_samplers
  q -At -f - <"$m8_dir/pg-stats.sql" >"$idir/pg-stats-after.json"
  q -At -f - <"$m8_dir/pg-statements.sql" >"$idir/pg-statements.json"
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
  if [ $((n % 2)) = 1 ]; then first=on second=off; else first=off second=on; fi
  run_side "$first" "$n"
  run_side "$second" "$n"
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
python3 -I - "$raw" "$items" "$(tr -d '\r\n' <"$bin/source-rev")" <<'PY'
import json, math, pathlib, re, statistics, sys
raw = pathlib.Path(sys.argv[1])

def histogram(prom, name):
    """Cumulative buckets (le -> count), count and sum of one histogram, summed over label sets."""
    buckets, count, total = {}, 0.0, 0.0
    if not prom.exists():
        return buckets, count, total
    for line in prom.read_text().splitlines():
        if line.startswith(name + '_bucket'):
            m = re.search(r'le="([^"]+)"\} (\S+)', line)
            le = float('inf') if m[1] == '+Inf' else float(m[1])
            buckets[le] = buckets.get(le, 0.0) + float(m[2])
        elif line.startswith(name + '_count'):
            count += float(line.rsplit(' ', 1)[1])
        elif line.startswith(name + '_sum'):
            total += float(line.rsplit(' ', 1)[1])
    return buckets, count, total

def quantile(buckets, q):
    ordered = sorted(buckets.items())
    if not ordered or ordered[-1][1] == 0:
        return None
    target, prev_le, prev_c = q * ordered[-1][1], 0.0, 0.0
    for le, c in ordered:
        if c >= target:
            if le == float('inf'):
                return prev_le * 1000
            return (prev_le + (le - prev_le) * (target - prev_c) / max(c - prev_c, 1e-9)) * 1000
        prev_le, prev_c = le, c

def wal_delta(directory):
    try:
        before = json.loads((directory / 'pg-stats-before.json').read_text())['pg_stat_wal']
        after = json.loads((directory / 'pg-stats-after.json').read_text())['pg_stat_wal']
    except (OSError, KeyError, ValueError):
        return None
    return {k: after[k] - before[k] for k in ('wal_bytes', 'wal_records', 'wal_fpi') if k in after and k in before}

def docker_cpu(directory):
    rows = {}
    path = directory / 'docker-stats.ndjson'
    if not path.exists():
        return {}
    for line in path.read_text().splitlines():
        try:
            r = json.loads(line)
            rows.setdefault(r['name'], []).append(float(r['cpu'].rstrip('%')))
        except (ValueError, KeyError):
            continue
    return {name: {'samples': len(v), 'cpu_mean_pct': statistics.fmean(v), 'cpu_max_pct': max(v)} for name, v in rows.items()}

runs = {'on': [], 'off': []}
for f in sorted(raw.glob('o[nf]*[0-9].json')):
    r = json.loads(f.read_text())
    inst = raw / (f.stem + '.instrument')
    buckets, count, total = histogram(raw / (f.stem + '.worker-metrics.prom'), 'zirv_flow_decision_append_seconds')
    r['decision_append'] = dict(count=count, mean_ms=total / count * 1000 if count else None,
        p50_ms=quantile(buckets, 0.5), p95_ms=quantile(buckets, 0.95), p99_ms=quantile(buckets, 0.99))
    r['wal'] = wal_delta(inst)
    r['docker_cpu'] = docker_cpu(inst)
    runs[r['side']].append(r)

def spread(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    return dict(n=len(values), mean=statistics.fmean(values), min=min(values), max=max(values),
                stdev=statistics.stdev(values) if len(values) > 1 else 0.0, values=values)

by_round = {side: {r['run']: r for r in rs} for side, rs in runs.items()}
paired_loss = []
for n in sorted(set(by_round['on']) & set(by_round['off'])):
    on_t, off_t = by_round['on'][n]['drain_seconds'], by_round['off'][n]['drain_seconds']
    paired_loss.append((on_t - off_t) / on_t)
on_t = statistics.fmean([r['drain_seconds'] for r in runs['on']]) if runs['on'] else None
off_t = statistics.fmean([r['drain_seconds'] for r in runs['off']]) if runs['off'] else None
on_append = spread([r['decision_append']['p95_ms'] for r in runs['on']])
summary = dict(runs=runs, source_rev=sys.argv[3], rounds=len(paired_loss),
    mean_drain_seconds=dict(on=on_t, off=off_t),
    throughput_loss=(on_t - off_t) / on_t if on_t and off_t else None,
    throughput_loss_per_round=spread(paired_loss), target_throughput_loss=0.10,
    added_p95_ms=on_append, off_append_observations=sum(r['decision_append']['count'] for r in runs['off']),
    target_added_p95_ms=5,
    wal_per_run=dict((side, [r['wal'] for r in rs]) for side, rs in runs.items()),
    measurement='added p95 is the p95 of the worker zirv_flow_decision_append_seconds histogram (one observation per decision append call, bucket interpolation) on the on build; the off build records none (off_append_observations). The throughput loss is (on - off) / on of the drain time, paired per round; rounds are interleaved and alternate the side that goes first. Per run: docker stats, pg_stat_wal and pg_stat_statements before and after the drain, and the wait sampler, in <side><n>.instrument/.',
    items=int(sys.argv[2]))
(raw / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
PY
restored=''
