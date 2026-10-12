#!/usr/bin/env bash
# Capacity benchmarks at doubling resource tiers (spec #47, runbook section 8).
#
#   scripts/bench/tier.sh <S|M|L> <test>
#
# Tests: track1 identify1 track4 (ceiling ramps), sustained, burst, and, for tier M only, batch,
# decappend and oq1. Every test recreates the stack on a fresh volume, so a test never sees an
# earlier test's data. The stack (PostgreSQL 16, serve, worker, scheduler) shares one cpuset and
# memory budget (compose.tiers.yaml); the load generator runs on the host, outside the budget.
#
# Environment: BENCH_LOADGEN (default target/release/loadgen), BENCH_SCRATCH (key files and raw
# generator reports, default /tmp/zirv-bench), BENCH_DATA (default docs/benchmarks/data).
# Needs bash, docker compose, jq, awk and curl.

set -uo pipefail

tier="${1:?tier S, M or L}"
test_name="${2:?test name}"
root="$(git rev-parse --show-toplevel)"
cd "$root"
loadgen="${BENCH_LOADGEN:-$root/target/release/loadgen}"
scratch="${BENCH_SCRATCH:-/tmp/zirv-bench}"
data="${BENCH_DATA:-$root/docs/benchmarks/data}/$tier"
mkdir -p "$scratch/raw" "$data"
pw="bench-pass-12345"          # local, throwaway stack only
step_s="${BENCH_STEP_SECONDS:-90}"   # seconds per ramp step
ramp_max="${BENCH_RAMP_MAX:-8000}"  # last ramp step, items/s
host="${BENCH_HOST:-127.0.0.1}"          # the generator's target; "localhost" costs ~140 ms of API p99 on Windows + Docker Desktop
ramp_min="${BENCH_RAMP_MIN:-250}"   # first ramp step, items/s
ramp_step="${BENCH_RAMP_STEP:-250}" # rung size, items/s

# Tier budget for zirv-flow only (serve, worker, scheduler): cpuset, then memory in MB for the
# worker (50%), serve (40%) and scheduler (10%). PostgreSQL is outside the budget (compose.tiers.yaml).
case "$tier" in
  S) cpuset="0-1"; set -- 2048 1638 410 ;;
  M) cpuset="0-3"; set -- 4096 3277 819 ;;
  L) cpuset="0-7"; set -- 8192 6554 1638 ;;
  *) echo "tier must be S, M or L" >&2; exit 2 ;;
esac
export BENCH_APP_CPUSET="$cpuset" BENCH_MEM_WORKER="${1}m" BENCH_MEM_SERVE="${2}m" BENCH_MEM_SCHEDULER="${3}m"
c="docker compose -f docker-compose.yml -f scripts/bench/compose.bench.yaml -f scripts/bench/compose.tiers.yaml"
log() { echo "[$tier/$test_name $(date -u +%T)] $*" >&2; }
die() { log "FAILED: $*"; stop_samplers; exit 1; }

pg=""
q() { docker exec -i "$pg" psql -U zirv -d zirv_flow "$@"; }

stack_up() {
  $c down -v >/dev/null 2>&1 || true
  $c up -d --wait --wait-timeout 300 >/dev/null 2>&1 || { log "stack did not come up"; exit 1; }
  pg="$($c ps -q postgres)"
  until curl -fsS localhost:8080/readyz >/dev/null 2>&1; do sleep 1; done
  echo "$pw" | $c exec -T zirv-flow zirv-flow admin create-owner --email bench@example.com >/dev/null
  headers="$(mktemp)"
  csrf=$(jq -n --arg p "$pw" '{email:"bench@example.com",password:$p}' \
    | curl -fsS -D "$headers" -X POST localhost:8080/api/v1/session -H 'content-type: application/json' --data-binary @- \
    | jq -r .csrf_token)
  session=$(tr -d '\r' <"$headers" | sed -nE 's/^[Ss]et-[Cc]ookie: (zf_session=[^;]+).*/\1/p' | head -n1)
  {
    echo "tier=$tier app_cpuset=$cpuset pg_cpuset=${BENCH_PG_CPUSET:-8-31}"
    echo "image=$(docker image inspect ghcr.io/glubiz/zirv-flow:latest --format '{{.Id}} created={{.Created}}')"
    docker inspect --format '{{.Name}} cpuset={{.HostConfig.CpusetCpus}} mem_limit_bytes={{.HostConfig.Memory}}' \
      $($c ps -q) | sort
    for s in server_version shared_buffers effective_cache_size maintenance_work_mem max_connections \
      synchronous_commit wal_level fsync work_mem max_wal_size checkpoint_timeout; do
      echo "pg.$s=$(q -Atc "show $s")"
    done
    $c exec -T zirv-flow printenv | grep '^ZIRV_FLOW_' | grep -v -E 'URL|SECRET|MASTER' | sort
  } >"$data/$test_name-stack.txt"
}

# mkkey <n>: creates workspace <n> and an ingest key in $scratch/key-<n>.
mkkey() {
  ws=$(curl -fsS -X POST localhost:8080/api/v1/workspaces -H "cookie: $session" -H "x-csrf-token: $csrf" \
    -H 'content-type: application/json' -d "{\"name\":\"bench$1\"}" | jq -r .id)
  curl -fsS -X POST "localhost:8080/api/v1/workspaces/$ws/api-keys" -H "cookie: $session" -H "x-csrf-token: $csrf" \
    -H 'content-type: application/json' -d '{"name":"loadgen","scopes":["ingest"]}' | jq -r .key >"$scratch/key-$1"
}

# Background samplers: docker stats of the four containers and the inbox, every ~5 s.
statsfile="" inboxfile="" samplers=()
start_samplers() {
  statsfile="$data/$test_name-docker-stats.csv"; inboxfile="$data/$test_name-inbox-samples.csv"
  echo "epoch_s,container,cpu_pct,mem_usage_limit" >"$statsfile"
  echo "epoch_s,inbox_rows,oldest_age_s" >"$inboxfile"
  ( while true; do
      ts=$(date +%s)
      docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}}' 2>/dev/null \
        | grep '^bench-' | sed "s/^/$ts,/" >>"$statsfile"
      sleep 2
    done ) & samplers+=($!)
  connfile="$data/$test_name-pg-connections.csv"
  echo "epoch_s,client_addr,connections" >"$connfile"
  ( while true; do
      q -Atc "select extract(epoch from now())::int||','||coalesce(client_addr::text,'local')||','||count(*) from pg_stat_activity where datname='zirv_flow' group by client_addr"         2>/dev/null >>"$connfile"
      sleep 5
    done ) & samplers+=($!)
  ( while true; do
      row=$(q -Atc "select count(*)||','||coalesce(round(extract(epoch from now()-min(received_at))::numeric,1),0) from ingest_inbox" 2>/dev/null)
      [ -z "$row" ] || echo "$(date +%s),$row" >>"$inboxfile"
      sleep 4
    done ) & samplers+=($!)
}
stop_samplers() { [ "${#samplers[@]}" -eq 0 ] || kill "${samplers[@]}" 2>/dev/null; samplers=(); }
trap 'stop_samplers' EXIT

# peaks <t0> <t1>: peak CPU% (of one core) and memory (MiB) per container in the window, as JSON.
peaks() {
  awk -F, -v a="$1" -v b="$2" '
    $1 >= a && $1 <= b {
      n = $2; sub(/^bench-/, "", n); sub(/-1$/, "", n); cpu = $3; sub(/%/, "", cpu)
      split($4, m, " "); v = m[1]
      if (v ~ /GiB$/) { sub(/GiB/, "", v); v = v * 1024 } else if (v ~ /MiB$/) { sub(/MiB/, "", v) }
      else if (v ~ /KiB$/) { sub(/KiB/, "", v); v = v / 1024 } else { v = 0 }
      if (cpu + 0 > pc[n]) pc[n] = cpu + 0
      if (v + 0 > pm[n]) pm[n] = v + 0
      seen[n] = 1 }
    END { printf "{"; f = 1
      for (n in seen) { printf "%s\"%s\":{\"cpu_pct\":%.0f,\"mem_mib\":%.0f}", (f ? "" : ","), n, pc[n], pm[n]; f = 0 }
      printf "}" }' "$statsfile"
}

# wait_empty <cap_s>: waits for the inbox to empty; prints the seconds waited, or "null".
wait_empty() {
  local started=$SECONDS
  while [ "$(q -Atc 'select count(*) from ingest_inbox')" != 0 ]; do
    [ $((SECONDS - started)) -lt "$1" ] || { echo null; return; }
    sleep 2
  done
  echo $((SECONDS - started))
}

# applied <since_iso> <kind>: "items,p50,p95,p99,max" of the applied-latency script from <since_iso>.
applied() {
  docker exec -i -e PGOPTIONS="-c bench.since=$1" "$pg" psql -U zirv -d zirv_flow -A --csv -f - \
    <scripts/bench/applied-latency.sql | awk -F, -v k="$2" '$1 == k { print $2 "," $3 "," $4 "," $5 "," $6 }'
}

now_iso() { q -Atc "select to_char(now() at time zone 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"; }

# sums/maxima of the generator reports of one step
totals() {
  jq -s '{achieved: (map(.total.achieved_rate) | add), accepted: (map(.total.items_accepted) | add),
          p99_schedule_ms: (map(.total.latency_from_schedule.p99_ms) | max),
          p99_service_ms: (map(.total.service_latency.p99_ms) | max),
          p99_lag_ms: (map(.total.dispatch_lag.p99_ms) | max),
          late_dispatch: (map(.total.late_dispatch) | add),
          s5xx: (map(.total.status.server_5xx) | add), s429: (map(.total.status.rate_limited_429) | add),
          other: (map(.total.status.other) | add), transport: (map(.total.status.transport_errors) | add),
          gave_up: (map(.total.gave_up_after_retries) | add)}' "$@"
}

# pg_facts <name>: deadlocks in the PostgreSQL log, the container addresses (to read the connection
# samples) and the error lines, after a test.
pg_facts() {
  {
    echo "deadlocks=$(docker logs "$pg" 2>&1 | grep -c 'deadlock detected')"
    for s in postgres zirv-flow zirv-flow-worker zirv-flow-scheduler; do
      echo "ip.$s=$(docker inspect "bench-$s-1" --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' 2>/dev/null)"
    done
    docker logs "$pg" 2>&1 | grep -E 'deadlock|ERROR' | head -20
  } >"$data/$1-pg-facts.txt"
}

# ramp <name> <mode> <workspaces> <people>
ramp() {
  local name="$1" mode="$2" nws="$3" people="$4" prev=0 ceiling=0 verdict_all=held
  stack_up; for i in $(seq 1 "$nws"); do mkkey "$i"; done
  start_samplers
  : >"$data/$name-steps.jsonl"
  for rate in $(seq "$ramp_min" "$ramp_step" "$ramp_max"); do
    drained=$(wait_empty 300)
    [ "$drained" != null ] || { log "inbox not empty before step $rate"; verdict_all="inbox not empty before step $rate"; break; }
    since=$(now_iso); t0=$(date +%s)
    per=$((rate / nws)); files=(); pids=()
    for i in $(seq 1 "$nws"); do
      f="$scratch/raw/$name-$rate-$i.json"; files+=("$f")
      "$loadgen" --url "http://$host:8080" --key-file "$scratch/key-$i" --mode "$mode" --rate "$per" \
        --duration "$step_s" --people "$people" --run-id "$name-$rate-$i" --out "$f" 2>/dev/null & pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p" || die "a load generator failed in step $rate"; done
    t1=$(date +%s)
    age_end=$(q -Atc "select coalesce(round(extract(epoch from now()-min(received_at))::numeric,1),0)||','||count(*) from ingest_inbox")
    age_mid=$(awk -F, -v a=$((t0 + step_s * 4 / 9)) -v b=$((t0 + step_s * 6 / 9)) '$1 >= a && $1 <= b && $3 + 0 > m { m = $3 + 0 } END { print m + 0 }' "$inboxfile")
    drain_s=$(wait_empty 180)
    tot=$(totals "${files[@]}")
    ap=$(applied "$since" "$mode")
    pk=$(peaks "$t0" "$t1")
    jq -nc --argjson tot "$tot" --argjson peaks "$pk" --arg ap "${ap:-}" --arg age "$age_end" \
      --argjson mid "$age_mid" --argjson drain "$drain_s" --argjson rate "$rate" --argjson nws "$nws" \
      --arg since "$since" '
      ($age | split(",")) as $a | ($a[0] | tonumber) as $age_end | ($a[1] | tonumber) as $rows_end
      | (if $ap == "" then [] else ($ap | split(",") | map(tonumber)) end) as $p
      | {target_rate: $rate, workspaces: $nws, since: $since} + $tot
        + {inbox_age_end_s: $age_end, inbox_rows_end: $rows_end, inbox_age_mid_s: $mid,
           drain_after_step_s: $drain,
           applied_items: ($p[0]), applied_p50_ms: ($p[1]), applied_p95_ms: ($p[2]), applied_p99_ms: ($p[3]),
           peaks: $peaks}
        | . + {reasons: ([
            (if .late_dispatch > 0 then "generator_late_dispatch" else empty end),
            (if .achieved < 0.98 * .target_rate then "achieved_below_target" else empty end),
            (if .p99_schedule_ms >= 100 then "api_p99_ge_100ms" else empty end),
            (if (.s5xx + .transport + .other + .s429) > 0 then "errors_or_429" else empty end),
            (if .inbox_age_end_s >= 5 then "inbox_age_ge_5s" else empty end),
            (if .inbox_age_end_s > .inbox_age_mid_s + 3 then "inbox_age_growing" else empty end)
          ])}
        | . + {held: (.reasons | length == 0), invalid_generator_bound: (.late_dispatch > 0)}' \
      >>"$data/$name-steps.jsonl" || die "the step summary failed"
    last=$(tail -n1 "$data/$name-steps.jsonl")
    log "step $rate: $(jq -c '{achieved, p99_schedule_ms, age_end: .inbox_age_end_s, applied_p95_ms, reasons}' <<<"$last")"
    if [ "$(jq -r .held <<<"$last")" = true ]; then ceiling=$rate; else break; fi
  done
  stop_samplers; pg_facts "$name"
  jq -nc --arg name "$name" --argjson ceiling "$ceiling" --arg note "$verdict_all" \
    '{test: $name, ceiling_items_per_s: $ceiling, note: $note}' >"$data/$name-ceiling.json"
  log "ceiling $ceiling"
}

# single <name> <wait_cap_s> <generator args...>: one generator run, then the drain to empty.
single() {
  local name="$1" cap="$2"; shift 2
  stack_up; mkkey 1; start_samplers
  since=$(now_iso); t0=$(date +%s)
  "$loadgen" --url "http://$host:8080" --key-file "$scratch/key-1" --run-id "$name" --out "$data/$name.json" "$@" \
    2>"$data/$name-generator.log" || die "the load generator failed"
  t1=$(date +%s)
  rows_end=$(q -Atc "select count(*)||','||coalesce(round(extract(epoch from now()-min(received_at))::numeric,1),0) from ingest_inbox")
  drain_s=$(wait_empty "$cap"); t2=$(date +%s)
  rest=$(q -Atc 'select count(*) from ingest_inbox')
  q -A --csv -c "select (select count(*) from events) events, (select count(*) from people) people, (select count(*) from decision_records) decisions, (select count(*) from ingest_rejects) rejects, '$since' since, $drain_s drain_s_after_generator_stopped, '$rows_end' inbox_rows_age_at_generator_stop, $rest inbox_rows_left" \
    >"$data/$name-counts.csv"
  docker exec -i -e PGOPTIONS="-c bench.since=$since" "$pg" psql -U zirv -d zirv_flow -A --csv -f - \
    <scripts/bench/applied-latency.sql >"$data/$name-applied-latency.csv" || die "applied-latency failed"
  peaks "$t0" "$t2" >"$data/$name-peaks.json"
  { curl -fsS localhost:9100/metrics; curl -fsS localhost:9101/metrics; curl -fsS localhost:8080/metrics; } 2>/dev/null \
    | grep -E '^zirv_flow_(ingest|job_duration|job_claim|jobs)' >"$data/$name-metrics.txt"
  stop_samplers; pg_facts "$name"
  log "generator done $((t1 - t0)) s; drain $drain_s s; inbox left $rest"
}

case "$test_name" in
  track1)    ramp track1 track 1 100000 ;;
  identify1) ramp identify1 identify 1 5000000 ;;
  track4)    ramp track4 track 4 100000 ;;
  sustained) single sustained 1800 --mode track --rate 2000 --duration 600 --people 100000 ;;
  burst)
    export BENCH_RATE_PER_KEY=20000 BENCH_BURST_PER_KEY=40000
    single burst 1800 --mode batch --batch-size 100 --burst-rate 10000 --burst-duration 120 --people 1000000 ;;
  batch)
    [ "$tier" = M ] || { echo "batch runs on tier M only" >&2; exit 2; }
    single batch 1800 --mode batch --batch-size 500 --rate 2000 --duration 600 --people 1000000 ;;
  decappend)
    [ "$tier" = M ] || { echo "decappend runs on tier M only" >&2; exit 2; }
    stack_up
    # Build the release test binary first (`cargo test --release -p zirv-flow-store --no-run`) and run
    # it directly, so no compile happens during the measurement.
    testbin="$(ls -t "$root"/target/release/deps/zirv_flow_store-*.exe 2>/dev/null | head -n1)"
    DATABASE_URL="postgres://zirv:zirv-quickstart@127.0.0.1:55432/postgres" ZIRV_FLOW_BENCH_DECISIONS=500 \
      "$testbin" a_drain_batch_of_decisions --ignored --nocapture 2>/dev/null \
      | grep '^{"benchmark":"decision_append"' >"$data/decision-append.json"; [ -s "$data/decision-append.json" ] || { log "decision-append produced no output"; exit 1; }
    cat "$data/decision-append.json" ;;
  oq1)
    [ "$tier" = M ] || { echo "oq1 runs on tier M only" >&2; exit 2; }
    export BENCH_PG_SHM=1gb   # parallel queries over a million rows need more than Docker's 64 MB
    stack_up
    PSQL="docker exec -i $pg psql" DATABASE_URL="postgres://zirv:zirv-quickstart@localhost:5432/zirv_flow" \
      OQ1_PLAN_DIR="$data/oq1-plans" bash scripts/bench/oq1/run.sh >"$data/oq1.json" || { log "oq1 failed"; exit 1; } ;;
  *) echo "unknown test $test_name" >&2; exit 2 ;;
esac
