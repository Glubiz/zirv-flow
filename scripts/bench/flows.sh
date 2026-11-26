#!/usr/bin/env bash
# Flow-execution benchmark: trigger event -> enrollment -> every step -> completion, under ingest
# load, on the tier L stack (README section 8, "Flow execution").
#
#   scripts/bench/flows.sh up                      fresh stack, owner, workspace, ingest key
#   scripts/bench/flows.sh seed [N=100000]         identify people, wait for the inbox to empty
#   scripts/bench/flows.sh setup                   create and publish the benchmark flow
#   scripts/bench/flows.sh window <label> <background_rate> <trigger_rate> [seconds=300]
#   scripts/bench/flows.sh ramp <background_rate> <start> <step> <max> [seconds_per_step=180]
#   scripts/bench/flows.sh down                    stop the stack and drop its volumes
#
# The stack is the tier L one of tier.sh, with PostgreSQL on cpuset 8-15 and a 16 GB limit (the
# values below; override with the BENCH_* variables before the first call). Environment:
# BENCH_LOADGEN (default target/release/loadgen), BENCH_SCRATCH (key, cookies, raw generator
# reports; default /tmp/zirv-bench-flows), BENCH_DATA (default docs/benchmarks/data/tier-l-1.0.38;
# results go to $BENCH_DATA/flows), BENCH_HOST (default 127.0.0.1), BENCH_FLOW_DRAIN_CAP (seconds a
# window waits for its enrollments to finish, default 300), BENCH_SEED_FACTOR (identify items per
# person, default 3), BENCH_SEED_RATE (default 2000). Needs bash, docker compose, jq, awk, curl.

set -uo pipefail

cmd="${1:-}"; [ $# -eq 0 ] || shift
root="$(git rev-parse --show-toplevel)"
cd "$root" || exit 2
here="$root/scripts/bench"
loadgen="${BENCH_LOADGEN:-$root/target/release/loadgen}"
scratch="${BENCH_SCRATCH:-/tmp/zirv-bench-flows}"
data="${BENCH_DATA:-$root/docs/benchmarks/data/tier-l-1.0.38}/flows"
host="${BENCH_HOST:-127.0.0.1}"
api="http://$host:8080/api/v1"
drain_cap="${BENCH_FLOW_DRAIN_CAP:-300}"
people="${BENCH_PEOPLE:-100000}"  # the id space of every generator (the smoke test seeds fewer)
wait_s=30              # the flow's wait
trig_event=bench_flow_trigger
pw="bench-pass-12345"  # local, throwaway stack only
email="bench@example.com"
mkdir -p "$scratch/raw" "$data"
chmod 700 "$scratch" 2>/dev/null || true

# The tier L stack with the PostgreSQL budget of this benchmark.
export BENCH_APP_CPUSET="${BENCH_APP_CPUSET:-0-7}" BENCH_MEM_WORKER="${BENCH_MEM_WORKER:-8192m}"
export BENCH_MEM_SERVE="${BENCH_MEM_SERVE:-6554m}" BENCH_MEM_SCHEDULER="${BENCH_MEM_SCHEDULER:-1638m}"
export BENCH_PG_CPUSET="${BENCH_PG_CPUSET:-8-15}" BENCH_PG_MEMORY="${BENCH_PG_MEMORY:-16g}"
export LC_NUMERIC=C
c="docker compose -f docker-compose.yml -f scripts/bench/compose.bench.yaml -f scripts/bench/compose.tiers.yaml"
project="$($c config --format json 2>/dev/null | jq -r .name)"; cprefix="$project-"
[ -n "$project" ] && [ "$project" != null ] || { echo "could not read the compose project name" >&2; exit 2; }
log() { echo "[flows/$cmd $(date -u +%T)] $*" >&2; }
die() { log "FAILED: $*"; stop_samplers; exit 1; }

pg=""
q() { docker exec -i "$pg" psql -U zirv -d zirv_flow -X "$@"; }
need_pg() { pg="$($c ps -q postgres)"; [ -n "$pg" ] || die "the stack is not up (run: flows.sh up)"; }
now_iso() { q -Atc "select to_char(now() at time zone 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"; }

state_get() { [ -f "$scratch/state.env" ] && sed -n "s/^$1=//p" "$scratch/state.env" | tail -n 1 || true; }
state_put() {
  touch "$scratch/state.env"
  { grep -v "^$1=" "$scratch/state.env" || true; echo "$1=$2"; } >"$scratch/state.env.tmp" && mv "$scratch/state.env.tmp" "$scratch/state.env"
}
load_state() { ws="$(state_get WS)"; flow="$(state_get FLOW)"; [ -n "$ws" ] || die "no workspace: run flows.sh up"; }

# ---- API (session cookie + CSRF, kept in $scratch, never under docs/) --------------------------
login() {
  local hdr body
  hdr="$(mktemp)"
  body="$(jq -n --arg e "$email" --arg p "$pw" '{email:$e,password:$p}' \
    | curl -fsS -D "$hdr" -X POST "$api/session" -H 'content-type: application/json' --data-binary @-)" \
    || { rm -f "$hdr"; return 1; }
  local csrf session
  csrf="$(jq -r .csrf_token <<<"$body")"
  session="$(tr -d '\r' <"$hdr" | sed -nE 's/^[Ss]et-[Cc]ookie: (zf_session=[^;]+).*/\1/p' | head -n1)"
  rm -f "$hdr"
  [ -n "$csrf" ] && [ -n "$session" ] || return 1
  ( umask 077; printf 'cookie: %s\nx-csrf-token: %s\n' "$session" "$csrf" >"$scratch/auth.hdr" )
}
# api METHOD PATH [JSON]: body on stdout; status >= 400 prints the problem on stderr and returns 1.
# A 409 is returned as 2 (exists) so that the idempotent creates can tell it from a failure.
api() {
  local out status body=()
  out="$(mktemp)"
  [ -z "${3:-}" ] || body=(--data-binary "$3")
  status="$(curl -sS -o "$out" -w '%{http_code}' -X "$1" "$api$2" -H "@$scratch/auth.hdr" \
    -H 'content-type: application/json' ${body[@]+"${body[@]}"})"
  if [ "$status" -ge 400 ]; then
    [ "$status" = 409 ] || echo "$1 $2 -> $status: $(head -c 600 "$out")" >&2
    rm -f "$out"
    [ "$status" = 409 ] && return 2
    return 1
  fi
  cat "$out"; rm -f "$out"
}

# ---- samplers ----------------------------------------------------------------------------------
samplers=()
stop_samplers() { [ "${#samplers[@]}" -eq 0 ] || kill "${samplers[@]}" 2>/dev/null; samplers=(); }
trap 'stop_samplers' EXIT

# start_samplers <label>: docker stats every ~2 s, queue/inbox/enrollment samples every 5 s (ndjson,
# with the host epoch added), wake-time samples every 2 s.
start_samplers() {
  statsfile="$data/$1-docker-stats.csv"; samplefile="$data/$1-samples.ndjson"
  echo "epoch_s,container,cpu_pct,mem_usage_limit" >"$statsfile"; : >"$samplefile"
  docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}}' 2>/dev/null \
    | grep "^$cprefix" | sed "s/^/$(date +%s),/" >>"$statsfile"
  [ "$(wc -l <"$statsfile")" -gt 1 ] || die "docker stats matched no container named ${cprefix}*"
  ( while true; do
      ts=$(date +%s)
      docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}}' 2>/dev/null \
        | grep "^$cprefix" | sed "s/^/$ts,/" >>"$statsfile"
      sleep 1
    done ) & samplers+=($!)
  ( while true; do
      h=$(date +%s)
      row=$(q -At -v flow="$flow" -f - <"$here/flows/sample.sql" 2>/dev/null)
      [ -z "$row" ] || jq -c --argjson h "$h" '. + {host_epoch: $h}' <<<"$row" >>"$samplefile"
      sleep 5
    done ) & samplers+=($!)
  ( while true; do
      q -q -f - <"$here/flows/wakes.sql" >/dev/null 2>&1
      sleep 2
    done ) & samplers+=($!)
}

# usage_json <t0> <t1>: per container, average and peak CPU in cores and peak memory (MiB).
usage_json() {
  awk -F, -v a="$1" -v b="$2" -v pre="$cprefix" '
    $1 >= a && $1 <= b && $1 != "epoch_s" {
      n = $2; sub("^" pre, "", n); sub(/-1$/, "", n); cpu = $3; sub(/%/, "", cpu); cpu = cpu / 100
      split($4, m, " "); v = m[1]
      if (v ~ /GiB$/) { sub(/GiB/, "", v); v = v * 1024 } else if (v ~ /MiB$/) { sub(/MiB/, "", v) }
      else if (v ~ /KiB$/) { sub(/KiB/, "", v); v = v / 1024 } else { v = 0 }
      sum[n] += cpu; cnt[n]++
      if (cpu > pc[n]) pc[n] = cpu
      if (v + 0 > pm[n]) pm[n] = v + 0 }
    END { printf "{"; f = 1
      for (n in cnt) { printf "%s\"%s\":{\"avg_cores\":%.2f,\"peak_cores\":%.2f,\"peak_mem_mib\":%.0f,\"samples\":%d}", (f ? "" : ","), n, sum[n] / cnt[n], pc[n], pm[n], cnt[n]; f = 0 }
      printf "}" }' "$statsfile"
}

# ---- worker metrics (job duration and claim lag per kind, enroll refusals) ---------------------
scrape() {
  { curl -fsS "http://$host:9100/metrics" | grep -E '^zirv_flow_(job_duration_seconds|job_claim_lag_seconds|flow_enroll_skips_total|flow_enrollments_total|jobs_total)'; } >"$1" 2>/dev/null || true
}
# hist_json <before> <after> <metric>: per flow.* kind, the delta count, mean and the upper bound
# (seconds) of the bucket holding the p50, p95 and p99 (null: above the last finite bucket).
hist_json() {
  awk -v m="$3" '
    function lab(s, k) { if (match(s, k "=\"[^\"]*\"")) return substr(s, RSTART + length(k) + 2, RLENGTH - length(k) - 3); return "" }
    function key(line) { return substr(line, 1, length(line) - length($NF) - 1) }
    FILENAME == ARGV[1] { before[key($0)] = $NF; next }
    {
      s = key($0); d = $NF - before[s]
      if (index(s, m "_bucket{") == 1) { k = lab(s, "kind"); l = lab(s, "le"); B[k, l] = d; if (!((k, l) in seen)) { seen[k, l] = 1; n[k]++; L[k, n[k]] = l } kinds[k] = 1 }
      else if (index(s, m "_sum{") == 1) { S[lab(s, "kind")] = d }
      else if (index(s, m "_count{") == 1) { C[lab(s, "kind")] = d }
    }
    function val(l) { return l == "+Inf" ? 1e300 : l + 0 }
    function q(k, p,   i, j, t, total, r, best) {
      total = B[k, "+Inf"]; r = int(p * total); if (r < p * total) r++; if (r < 1) r = 1
      for (i = 1; i <= n[k]; i++) for (j = i + 1; j <= n[k]; j++) if (val(L[k, j]) < val(L[k, i])) { t = L[k, i]; L[k, i] = L[k, j]; L[k, j] = t }
      for (i = 1; i <= n[k]; i++) if (B[k, L[k, i]] >= r) return (L[k, i] == "+Inf" ? "null" : L[k, i])
      return "null" }
    END { printf "{"; f = 1
      for (k in kinds) {
        if (k !~ /^flow\./ || C[k] <= 0) continue
        printf "%s\"%s\":{\"count\":%d,\"mean_s\":%.4f,\"p50_le_s\":%s,\"p95_le_s\":%s,\"p99_le_s\":%s}", (f ? "" : ","), k, C[k], S[k] / C[k], q(k, 0.5), q(k, 0.95), q(k, 0.99); f = 0 }
      printf "}" }' "$1" "$2"
}
# counter_json <before> <after> <metric> <label>: delta per value of <label>, as {value: delta}.
counter_json() {
  awk -v m="$3" -v l="$4" '
    function lab(s, k) { if (match(s, k "=\"[^\"]*\"")) return substr(s, RSTART + length(k) + 2, RLENGTH - length(k) - 3); return "" }
    function key(line) { return substr(line, 1, length(line) - length($NF) - 1) }
    FILENAME == ARGV[1] { before[key($0)] = $NF; next }
    { s = key($0); if (index(s, m "{") == 1) { v = lab(s, l); D[v] += $NF - before[s] } }
    END { printf "{"; f = 1; for (v in D) if (D[v] != 0) { printf "%s\"%s\":%d", (f ? "" : ","), v, D[v]; f = 0 } printf "}" }' "$1" "$2"
}

# ---- waits -------------------------------------------------------------------------------------
# wait_empty <cap_s>: waits for the inbox to empty; prints the seconds waited, or "null".
wait_empty() {
  local started=$SECONDS
  while [ "$(q -Atc 'select count(*) from ingest_inbox')" != 0 ]; do
    [ $((SECONDS - started)) -lt "$1" ] || { echo null; return; }
    sleep 2
  done
  echo $((SECONDS - started))
}
# idle_sql: nothing left to do for the flow: no inbox row, no queued or running flow.enroll job,
# no enrollment in flight. Prints the number of such things (0 is idle).
idle_sql() {
  q -Atc "select (select count(*) from ingest_inbox) + (select count(*) from jobs where kind = 'flow.enroll' and state in ('ready','running')) + (select count(*) from enrollments where flow_id = '$flow' and state in ('active','waiting'))"
}
# wait_idle <cap_s>: seconds waited, or "null" at the cap.
wait_idle() {
  local started=$SECONDS
  while [ "$(idle_sql)" != 0 ]; do
    [ $((SECONDS - started)) -lt "$1" ] || { echo null; return; }
    sleep 2
  done
  echo $((SECONDS - started))
}

# ---- up / down ---------------------------------------------------------------------------------
cmd_up() {
  $c down -v >/dev/null 2>&1 || true
  $c up -d --wait --wait-timeout 300 >/dev/null 2>&1 || die "stack did not come up"
  pg="$($c ps -q postgres)"
  until curl -fsS "http://$host:8080/readyz" >/dev/null 2>&1; do sleep 1; done
  echo "$pw" | $c exec -T zirv-flow zirv-flow admin create-owner --email "$email" >/dev/null || die "create-owner failed"
  login || die "cannot sign in"
  local ws key
  ws="$(api POST /workspaces '{"name":"benchflows"}' | jq -r .id)"; [ -n "$ws" ] && [ "$ws" != null ] || die "workspace not created"
  key="$(api POST "/workspaces/$ws/api-keys" '{"name":"loadgen","scopes":["ingest"]}' | jq -r .key)"
  [ -n "$key" ] && [ "$key" != null ] || die "ingest key not created"
  ( umask 077; printf '%s' "$key" >"$scratch/key-1" )
  rm -f "$scratch/state.env"; state_put WS "$ws"
  q -v ON_ERROR_STOP=1 -q -f - <"$here/flows/init.sql" >/dev/null || die "flowbench schema not created"
  {
    echo "test=flows tier=L app_cpuset=$BENCH_APP_CPUSET pg_cpuset=$BENCH_PG_CPUSET pg_memory=$BENCH_PG_MEMORY"
    echo "mem_worker=$BENCH_MEM_WORKER mem_serve=$BENCH_MEM_SERVE mem_scheduler=$BENCH_MEM_SCHEDULER"
    echo "image=$(docker image inspect ghcr.io/glubiz/zirv-flow:latest --format '{{.Id}} created={{.Created}}')"
    echo "version=$($c exec -T zirv-flow zirv-flow --version 2>/dev/null | tr -d '\r')"
    docker inspect --format '{{.Name}} cpuset={{.HostConfig.CpusetCpus}} mem_limit_bytes={{.HostConfig.Memory}}' \
      $($c ps -q) | sort
    for s in server_version shared_buffers effective_cache_size maintenance_work_mem max_connections \
      synchronous_commit wal_level fsync work_mem max_wal_size checkpoint_timeout; do
      echo "pg.$s=$(q -Atc "show $s")"
    done
    echo "serve_db_max_connections=${BENCH_SERVE_DB_CONNECTIONS:-10} (BENCH_SERVE_DB_CONNECTIONS)"
    docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' $($c ps -q zirv-flow) | tr -d '\r' | grep '^ZIRV_FLOW_' | grep -v -E 'URL|SECRET|MASTER' | sort
  } >"$data/stack.txt"
  log "stack up; workspace $ws; facts in $data/stack.txt"
}

cmd_down() {
  $c down -v >/dev/null 2>&1 || true
  rm -f "$scratch/key-1" "$scratch/auth.hdr" "$scratch/state.env"
  log "stack down, volumes removed, key and session removed"
}

# ---- registry, seed, setup ---------------------------------------------------------------------
# The flow compiles its branch and trait nodes against the workspace registry, so the traits and the
# trigger event must exist (and the traits before the identifies, so their values are stored).
ensure_registry() {
  local t rc
  for t in plan:string seats:number score:number bench_flow_tier:string bench_flow_runs:number; do
    api POST "/workspaces/$ws/traits" "$(jq -nc --arg k "${t%%:*}" --arg t "${t##*:}" '{key:$k,type:$t}')" >/dev/null; rc=$?
    [ "$rc" -le 2 ] || die "trait ${t%%:*} not created"
  done
  api POST "/workspaces/$ws/event-schemas" "$(jq -nc --arg n "$trig_event" '{name:$n,mode:"allow",properties:[]}')" >/dev/null; rc=$?
  [ "$rc" -le 2 ] || die "event schema not created"
  api POST "/workspaces/$ws/event-schemas/$trig_event/versions" >/dev/null; rc=$?
  [ "$rc" -le 2 ] || die "event schema not published"
}

cmd_seed() {
  local n="${1:-100000}" factor="${BENCH_SEED_FACTOR:-3}" rate="${BENCH_SEED_RATE:-2000}"
  need_pg; load_state; login || die "cannot sign in"
  ensure_registry
  # Each item names a random person, so `factor` items per person cover 1 - e^-factor of them
  # (95% at 3); the rest are created by their first trigger event, with no traits.
  local items=$((n * factor)) dur
  dur=$(( (items + rate - 1) / rate ))
  log "identify: $items items at $rate/s over $dur s into $n people"
  "$loadgen" --url "http://$host:8080" --key-file "$scratch/key-1" --mode identify --rate "$rate" \
    --duration "$dur" --people "$n" --seed 7 --run-id seed --out "$data/seed.json" 2>"$scratch/raw/seed.log" \
    || die "the load generator failed"
  local drained; drained="$(wait_empty 1800)"
  [ "$drained" != null ] || die "inbox not empty after 1800 s"
  log "ANALYZE"; q -qc "ANALYZE" >/dev/null || die "ANALYZE failed"
  jq -nc --argjson n "$n" --argjson drain "$drained" --argjson people "$(q -Atc 'select count(*) from people')" \
    --slurpfile g "$data/seed.json" \
    '{target_people: $n, people_in_db: $people, drain_s_after_generator: $drain,
      achieved_rate: $g[0].total.achieved_rate, accepted: $g[0].total.items_accepted, late_dispatch: $g[0].total.late_dispatch}' \
    | tee "$data/seed-summary.json" >&2
}

flow_definition() { # <list id>
  jq -nc --arg list "$1" --arg trig "$trig_event" --argjson wait "$wait_s" '{
    name: "bench-flow",
    graph: {v: 1,
      nodes: [
        {id: "b1", type: "branch", conditions: [{trait: "plan", op: "eq", value: "pro"}]},
        {id: "a_trait", type: "action.update_trait", key: "bench_flow_tier", change: {op: "set", value: "priority"}},
        {id: "a_list", type: "action.list", list_id: $list, change: "add"},
        {id: "w1", type: "wait.duration", seconds: $wait},
        {id: "a_after", type: "action.update_trait", key: "bench_flow_runs", change: {op: "increment", by: 1}},
        {id: "x1", type: "exit"}],
      edges: [
        {id: "e1", from: "b1", port: "case_0", to: "a_trait"},
        {id: "e2", from: "b1", port: "else", to: "a_list"},
        {id: "e3", from: "a_trait", port: "next", to: "w1"},
        {id: "e4", from: "a_list", port: "next", to: "w1"},
        {id: "e5", from: "w1", port: "next", to: "a_after"},
        {id: "e6", from: "a_after", port: "next", to: "x1"}]},
    settings: {trigger: {type: "event", event: $trig}, reentry: {mode: "once_at_a_time"}}}'
}

cmd_setup() {
  need_pg; load_state; login || die "cannot sign in"
  ensure_registry
  q -v ON_ERROR_STOP=1 -q -f - <"$here/flows/init.sql" >/dev/null || die "flowbench schema not created"
  local list body id
  list="$(api POST "/workspaces/$ws/lists" '{"name":"bench-flow-members"}' | jq -r .id)"
  [ -n "$list" ] && [ "$list" != null ] || die "list not created"
  body="$(flow_definition "$list")"
  id="$(api POST "/workspaces/$ws/flows" "$body" | jq -r .id)"
  [ -n "$id" ] && [ "$id" != null ] || die "flow not created"
  # Warm-up off BEFORE the publish, which would start its window (it is on for 5 minutes by default).
  api PUT "/workspaces/$ws/flows/$id/warmup" '{"enabled":false,"seconds":300}' >/dev/null || die "warm-up not turned off"
  api POST "/workspaces/$ws/flows/$id/publish" >/dev/null || die "publish failed"
  local detail; detail="$(api GET "/workspaces/$ws/flows/$id")" || die "cannot read the flow"
  [ "$(jq -r .state <<<"$detail")" = published ] || die "the flow is not published"
  [ "$(jq -r .warmup.enabled <<<"$detail")" = false ] || die "the warm-up is still on"
  q -qc "ANALYZE" >/dev/null || die "ANALYZE failed"
  state_put FLOW "$id"; state_put LIST "$list"
  jq -n --argjson d "$body" --arg flow "$id" --arg list "$list" --argjson warm "$(jq .warmup <<<"$detail")" \
    '{flow_id: $flow, list_id: $list, warmup: $warm, request: $d}' >"$data/flow-definition.json"
  log "flow $id published (warm-up off); definition in $data/flow-definition.json"
}

# ---- window ------------------------------------------------------------------------------------
# gen_json <report> <target>: the generator summary, or null when it did not run.
gen_json() {
  [ -s "$1" ] || { echo null; return; }
  jq -c --argjson target "$2" '.total | {target_rate: $target, items_offered, items_accepted, achieved_rate,
    late_dispatch, abandoned_requests, gave_up_after_retries, retries_after_429, status,
    api_p50_from_schedule_ms: .latency_from_schedule.p50_ms, api_p99_from_schedule_ms: .latency_from_schedule.p99_ms,
    dispatch_lag_p99_ms: .dispatch_lag.p99_ms}' "$1"
}

# run_window <label> <background_rate> <trigger_rate> <seconds>: the result is $data/<label>.json.
run_window() {
  local label="$1" bg="$2" trig="$3" secs="$4"
  need_pg; load_state; [ -n "$flow" ] || die "no flow: run flows.sh setup"
  local raw="$scratch/raw/$label"; mkdir -p "$raw"
  local idle; idle="$(wait_idle 300)"; [ "$idle" != null ] || die "the flow is not idle before $label (inbox, flow.enroll jobs or active enrollments left)"
  local seed; seed=$(cksum <<<"$label" | cut -d' ' -f1)
  scrape "$raw/metrics-before.txt"
  start_samplers "$label"
  local since t0h t1h t1 t2h pids=() drain_s
  since="$(now_iso)"; t0h=$(date +%s)
  if [ "$bg" -gt 0 ]; then
    "$loadgen" --url "http://$host:8080" --key-file "$scratch/key-1" --mode track --rate "$bg" --duration "$secs" \
      --people "$people" --seed "$seed" --run-id "$label-bg" --out "$raw/bg.json" 2>"$raw/bg.log" & pids+=($!)
  fi
  if [ "$trig" -gt 0 ]; then
    "$loadgen" --url "http://$host:8080" --key-file "$scratch/key-1" --mode track --rate "$trig" --duration "$secs" \
      --people "$people" --seed $((seed + 1)) --event "$trig_event" --run-id "$label-trig" --out "$raw/trig.json" 2>"$raw/trig.log" & pids+=($!)
  fi
  local p; for p in "${pids[@]}"; do wait "$p" || die "a load generator failed (see $raw/*.log)"; done
  t1h=$(date +%s); t1="$(now_iso)"
  log "generators done after $((t1h - t0h)) s; waiting for the enrollments (cap $drain_cap s)"
  drain_s="$(wait_idle "$drain_cap")"
  sleep 5   # one more sample after the end
  t2h=$(date +%s)
  stop_samplers
  scrape "$raw/metrics-after.txt"

  q -At -v ON_ERROR_STOP=1 -v ws="$ws" -v flow="$flow" -v since="$since" -v t1="$t1" -v secs="$secs" -v wait_s="$wait_s" \
    -v trig="$trig_event" -v prefix="lg-$label-trig-" -f - <"$here/flows/metrics.sql" >"$raw/sql.json" || die "metrics.sql failed"
  local applied
  applied="$(docker exec -i -e PGOPTIONS="-c bench.since=$since" "$pg" psql -U zirv -d zirv_flow -A --csv -f - \
    <"$here/applied-latency.sql" | awk -F, '$1 == "track" { print $2 "," $3 "," $4 "," $5 "," $6 }')"
  jq -s --argjson t0 "$t0h" --argjson t1 "$t1h" --argjson t2 "$t2h" '
    def mx: if length == 0 then null else max end;
    ([.[] | select(.host_epoch >= $t0 and .host_epoch <= $t2)]) as $all
    | ([$all[] | select(.host_epoch <= $t1)]) as $gen
    | ([$gen[] | select(.host_epoch >= $t0 + ($t1 - $t0) * 4 / 9 and .host_epoch <= $t0 + ($t1 - $t0) * 6 / 9) | (.oldest_due_age_s // 0)] | mx) as $mid
    | {samples: ($all | length),
       due_ready_peak: ($all | map(.due_ready) | mx), due_ready_final: ($all | last | .due_ready),
       running_peak: ($all | map(.running) | mx),
       oldest_due_age_peak_s: ($all | map(.oldest_due_age_s // 0) | mx),
       oldest_due_age_final_s: ($all | last | (.oldest_due_age_s // 0)),
       oldest_due_age_at_generators_stop_s: ($gen | last | (.oldest_due_age_s // 0)),
       oldest_due_age_mid_window_s: ($mid // 0),
       inbox_rows_peak: ($all | map(.inbox_rows) | mx), inbox_oldest_age_peak_s: ($all | map(.inbox_oldest_age_s // 0) | mx),
       enrollment_states_final: ($all | last | .enrollments)}' "$samplefile" >"$raw/backlog.json" || die "sample summary failed"

  jq -n --arg label "$label" --argjson bg "$bg" --argjson trig "$trig" --argjson secs "$secs" \
    --argjson wait "$wait_s" --arg flow "$flow" --arg drain "$drain_s" --argjson cap "$drain_cap" \
    --arg applied "$applied" --slurpfile sql "$raw/sql.json" --slurpfile backlog "$raw/backlog.json" \
    --argjson tg "$(gen_json "$raw/trig.json" "$trig")" --argjson bgg "$(gen_json "$raw/bg.json" "$bg")" \
    --argjson usage_gen "$(usage_json "$t0h" "$t1h")" --argjson usage_all "$(usage_json "$t0h" "$t2h")" \
    --argjson dur "$(hist_json "$raw/metrics-before.txt" "$raw/metrics-after.txt" zirv_flow_job_duration_seconds)" \
    --argjson lag "$(hist_json "$raw/metrics-before.txt" "$raw/metrics-after.txt" zirv_flow_job_claim_lag_seconds)" \
    --argjson skips "$(counter_json "$raw/metrics-before.txt" "$raw/metrics-after.txt" zirv_flow_flow_enroll_skips_total reason)" '
    ($sql[0]) as $s
    | ($applied | if . == "" then null else split(",") | map(tonumber? // null) | {items: .[0], p50_ms: .[1], p95_ms: .[2], p99_ms: .[3], max_ms: .[4]} end) as $ap
    | {label: $label, flow_id: $flow, since: $s.since, generator_seconds: $secs, wait_s: $wait,
       target_background_rate: $bg, target_trigger_rate: $trig,
       waited_for_enrollments_s: ($drain | if . == "null" then null else tonumber end), drain_cap_s: $cap,
       trigger_generator: $tg, background_generator: $bgg,
       triggers: {offered: ($tg.items_offered // 0), accepted: ($tg.items_accepted // 0), stored_as_events: $s.counts.trigger_events_stored},
       counts: $s.counts,
       latency_s: {
         trigger_to_enrollment: $s.seconds.trigger_to_enrollment,
         end_to_end: $s.seconds.end_to_end,
         end_to_end_minus_wait: $s.seconds.end_to_end_minus_wait,
         wake_lateness: $s.seconds.wake_lateness,
         wake_lateness_captured_only: $s.seconds.wake_lateness_captured_only},
       nodes_queue_delay_s: ($s.seconds | with_entries(select(.key | startswith("queue:"))) | with_entries(.key |= ltrimstr("queue:"))),
       job_metrics_delta: {duration_seconds: $dur, claim_lag_seconds: $lag, enroll_skips_by_reason: $skips},
       throughput: $s.throughput, step_outcomes: $s.step_outcomes,
       ingest_applied_latency_ms_track: $ap,
       containers: {during_generators: $usage_gen, including_drain: $usage_all},
       backlog: $backlog[0]}' >"$data/$label.json" || die "assembling $label.json failed"
  log "wrote $data/$label.json"
}

cmd_window() {
  local label="${1:?label}" bg="${2:?background rate}" trig="${3:?trigger rate}" secs="${4:-300}"
  run_window "$label" "$bg" "$trig" "$secs"
  jq -c '{label, enrolled: .counts.enrollments_created, completed: .counts.completed, refused: .counts.refused_total,
    t2e_p95: .latency_s.trigger_to_enrollment.p95, e2e_p95: .latency_s.end_to_end.p95, wake_late_p95: .latency_s.wake_lateness.p95,
    enr_per_s: .throughput.enrollments_per_s, backlog: .backlog.due_ready_peak}' "$data/$label.json"
}

# ---- ramp --------------------------------------------------------------------------------------
# reasons <window json>: the failure reasons of a ramp step (empty array: the step held).
step_reasons() {
  jq -c '
    (.triggers.accepted) as $acc | (.counts.refused_reentry) as $re | (.target_trigger_rate) as $t
    | [ (if ((.latency_s.trigger_to_enrollment.p95 // 1e9) > 60) then "trigger_to_enrollment_p95_gt_60s" else empty end),
        (if (.counts.enrollments_created < 0.95 * ($acc - $re)) then "enrollments_below_95pct_of_accepted" else empty end),
        (if (.backlog.oldest_due_age_at_generators_stop_s > 30 and .backlog.oldest_due_age_at_generators_stop_s > .backlog.oldest_due_age_mid_window_s)
           then "oldest_due_job_age_gt_30s_and_rising" else empty end),
        (if ((.trigger_generator.late_dispatch // 0) > 0) then "generator_late_dispatch" else empty end),
        (if ((.trigger_generator.achieved_rate // 0) < 0.98 * $t) then "generator_achieved_below_98pct" else empty end) ]' "$1"
}

cmd_ramp() {
  local bg="${1:?background rate}" start="${2:?start rate}" step="${3:?step}" max="${4:?max rate}" secs="${5:-180}"
  local out="$data/ramp-$bg.json" steps="$scratch/raw/ramp-$bg-steps.jsonl" rate verdict="held to the maximum rate" ceiling=0
  : >"$steps"
  for rate in $(seq "$start" "$step" "$max"); do
    run_window "ramp-$bg-$rate" "$bg" "$rate" "$secs"
    local f="$data/ramp-$bg-$rate.json" reasons
    reasons="$(step_reasons "$f")"
    jq -c --argjson r "$reasons" '{trigger_rate: .target_trigger_rate, held: ($r | length == 0), reasons: $r,
      accepted: .triggers.accepted, enrollments_created: .counts.enrollments_created, refused: .counts.refused_by_reason,
      completed: .counts.completed, still_active: .counts.still_active_or_waiting, failed: .counts.failed,
      trigger_to_enrollment_s: .latency_s.trigger_to_enrollment, end_to_end_minus_wait_s: .latency_s.end_to_end_minus_wait,
      wake_lateness_s: .latency_s.wake_lateness, enrollments_per_s: .throughput.enrollments_per_s,
      step_executions_per_s: .throughput.step_executions_per_s, backlog: .backlog, trigger_generator: .trigger_generator,
      background_generator: .background_generator, ingest_applied_latency_ms_track: .ingest_applied_latency_ms_track,
      containers: .containers.during_generators, result_file: input_filename}' "$f" >>"$steps"
    log "step $rate/s: $(tail -n1 "$steps" | jq -c '{held, reasons, enrollments_created, enrollments_per_s, p95: .trigger_to_enrollment_s.p95}')"
    if [ "$reasons" = "[]" ]; then ceiling=$rate; else verdict="failed at $rate triggers/s: $(jq -r 'join(", ")' <<<"$reasons")"; break; fi
  done
  jq -s --arg v "$verdict" --argjson ceiling "$ceiling" --argjson bg "$bg" --argjson secs "$secs" \
    '{background_rate: $bg, seconds_per_step: $secs, last_held_trigger_rate: $ceiling, verdict: $v, steps: .}' "$steps" >"$out"
  log "$verdict (last held: $ceiling); wrote $out"
}

case "$cmd" in
  up) cmd_up ;;
  seed) cmd_seed "$@" ;;
  setup) cmd_setup ;;
  window) cmd_window "$@" ;;
  ramp) cmd_ramp "$@" ;;
  down) cmd_down ;;
  *) echo "usage: flows.sh up | seed [N] | setup | window <label> <bg> <trig> [s] | ramp <bg> <start> <step> <max> [s] | down" >&2; exit 2 ;;
esac
