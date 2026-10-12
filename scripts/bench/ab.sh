#!/usr/bin/env bash
# Decision-log overhead A/B (runbook section 6): drains the same pre-loaded backlog with a native
# release build that writes decisions (side "on") and one built with bench-no-decisions (side "off"),
# against the same PostgreSQL 16 (the tier PostgreSQL configuration, on its own cores), and prints
# the applied items per second. Neither build is resource-limited.
#
#   scripts/bench/ab.sh <on|off> <run-number> <zirv-flow-binary> <loadgen-binary>
#
# Steps: fresh PostgreSQL volume, native `serve` (migrates), owner, workspace and ingest key, a
# backlog of ITEMS track events with no worker running, then the worker and scheduler are started and
# the time until the inbox is empty is measured. Output: $BENCH_DATA/ab/<side><n>.json.
# Windows host: native processes are started and stopped through PowerShell by exact PID.

set -uo pipefail
side="${1:?on|off}"; n="${2:?run number}"; exe="${3:?zirv-flow binary}"; loadgen="${4:?loadgen binary}"
root="$(git rev-parse --show-toplevel)"; cd "$root"
data="${BENCH_DATA:-$root/docs/benchmarks/data}/ab"; mkdir -p "$data"
scratch="${BENCH_SCRATCH:-/tmp/zirv-bench}"; mkdir -p "$scratch"
items="${AB_ITEMS:-600000}"
export BENCH_APP_CPUSET=0-1 BENCH_MEM_WORKER=1m BENCH_MEM_SERVE=1m BENCH_MEM_SCHEDULER=1m   # unused: only postgres runs
c="docker compose -f docker-compose.yml -f scripts/bench/compose.bench.yaml -f scripts/bench/compose.tiers.yaml"
pw=bench-pass-12345

$c down -v >/dev/null 2>&1
$c up -d --wait --wait-timeout 180 postgres >/dev/null 2>&1 || { echo "postgres did not start" >&2; exit 1; }
pg="$($c ps -q postgres)"
q() { docker exec -i "$pg" psql -U zirv -d zirv_flow "$@"; }

export ZIRV_FLOW_DATABASE__URL="postgres://zirv:zirv-quickstart@127.0.0.1:55432/zirv_flow"
export ZIRV_FLOW_SECRETS__MASTER_KEY_FILE="$scratch/ab-master.key" ZIRV_FLOW_DATABASE__MIGRATE_ON_START=true
export ZIRV_FLOW_SERVER__BIND=127.0.0.1:18080 ZIRV_FLOW_SERVER__COOKIE_SECURE=false ZIRV_FLOW_LOG__FILTER=warn
export ZIRV_FLOW_WORKER__CONCURRENCY=4 ZIRV_FLOW_LOG__FORMAT=text
rm -f "$scratch/ab-master.key"; "$exe" admin generate-master-key --out "$scratch/ab-master.key" >/dev/null

# start <name> <role> [ops-bind]: starts a native role through PowerShell and records its PID.
start() {
  local env_ops=""; [ -z "${3:-}" ] || env_ops="\$env:ZIRV_FLOW_OPS__BIND='$3';"
  local pidfile="$scratch/ab-$1.pid"; rm -f "$pidfile"
  # The PID goes to a file, not stdout: the started process would otherwise keep a command
  # substitution's pipe open and the call would never return.
  powershell -NoProfile -Command "$env_ops \$p = Start-Process -FilePath '$(cygpath -w "$exe")' -ArgumentList '$2' -PassThru -WindowStyle Hidden -RedirectStandardOutput '$(cygpath -w "$scratch")\ab-$1.out' -RedirectStandardError '$(cygpath -w "$scratch")\ab-$1.err'; Set-Content -Path '$(cygpath -w "$pidfile")' -Value \$p.Id" >/dev/null 2>&1
tr -d '[:space:]' <"$pidfile"
}
stop() { [ -z "${1:-}" ] || powershell -NoProfile -Command "Stop-Process -Id $1 -ErrorAction SilentlyContinue"; }

serve_pid=$(start serve serve)
until curl -fsS 127.0.0.1:18080/readyz >/dev/null 2>&1; do sleep 1; done
echo "$pw" | "$exe" admin create-owner --email bench@example.com >/dev/null
headers="$(mktemp)"
csrf=$(jq -n --arg p "$pw" '{email:"bench@example.com",password:$p}' | curl -fsS -D "$headers" -X POST 127.0.0.1:18080/api/v1/session -H 'content-type: application/json' --data-binary @- | jq -r .csrf_token)
session=$(tr -d '\r' <"$headers" | sed -nE 's/^[Ss]et-[Cc]ookie: (zf_session=[^;]+).*/\1/p' | head -n1)
ws=$(curl -fsS -X POST 127.0.0.1:18080/api/v1/workspaces -H "cookie: $session" -H "x-csrf-token: $csrf" -H 'content-type: application/json' -d '{"name":"ab"}' | jq -r .id)
curl -fsS -X POST "127.0.0.1:18080/api/v1/workspaces/$ws/api-keys" -H "cookie: $session" -H "x-csrf-token: $csrf" -H 'content-type: application/json' -d '{"name":"loadgen","scopes":["ingest"]}' | jq -r .key >"$scratch/ab-key"

# The backlog: no worker is running, so the inbox only fills.
"$loadgen" --url http://127.0.0.1:18080 --key-file "$scratch/ab-key" --mode track --rate 5000 \
  --duration $((items / 5000)) --people 100000 --run-id "ab-$side-$n" --out "$scratch/ab-load.json" 2>/dev/null
backlog=$(q -Atc 'select count(*) from ingest_inbox')

worker_pid=$(start worker worker 127.0.0.1:19100)
sched_pid=$(start scheduler scheduler 127.0.0.1:19101)
t0=$(date +%s.%N)
until [ "$(q -Atc 'select count(*) from ingest_inbox')" = 0 ]; do sleep 1; done
t1=$(date +%s.%N)
drain_s=$(awk -v a="$t1" -v b="$t0" 'BEGIN{printf "%.1f", a-b}')
decisions=$(q -Atc "select count(*) from decision_records where action <> 'job.attempt'")
events=$(q -Atc 'select count(*) from events')
stop "$worker_pid"; stop "$sched_pid"; stop "$serve_pid"
jq -nc --arg side "$side" --argjson n "$n" --argjson backlog "$backlog" --argjson drain "$drain_s" \
  --argjson decisions "$decisions" --argjson events "$events" \
  '{side: $side, run: $n, backlog_items: $backlog, drain_seconds: $drain, applied_items_per_s: ($events / $drain),
    events_stored: $events, decision_records: $decisions}' | tee "$data/$side$n.json"
$c down -v >/dev/null 2>&1
