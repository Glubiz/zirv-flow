#!/usr/bin/env bash
# One run of the M6 send-throughput benchmark (#233, architecture 19): enroll PEOPLE people in a
# fresh flow whose `action.send` node sends the template through the SMTP destination into the
# host sink, and measure. Run stack.sh up, sink.sh start, setup.sh and seed.sh first.
#
# A run: (1) recreates the workers (WORKERS processes) so their metrics start at zero, (2) applies
# the variant (send limits, destination rate), (3) creates a list and a flow and starts the flow
# for the list, (4) samples `docker stats` and waits until the sink has PEOPLE messages (or stalls
# for 180 s, or RUN_TIMEOUT_S passes), (5) scrapes every worker's /metrics, exports the enrollment
# times and the message outcomes, and (6) runs `sendbench report`.
#
# Variants, all environment switches:
#   PEOPLE        people enrolled, default 5000 (they must be seeded: seed.sh)
#   WORKERS       worker processes, default 1 (at most 10); WORKER_CONCURRENCY per process, default 4
#   TEMPLATE      blocks (default) or html: the render path of #236
#   DEST_RATE     the destination's rate and burst, messages per second; default 1000 (above the
#                 300 target, so the bucket is not the limit); 300 shows it holds at the target
#   LIMITS        off (default) or on: a frequency cap (1,000,000 per day, never reached) and quiet
#                 hours (a window on a weekday three days off, never reached), so the gates run
#                 and their cost shows without blocking a message
#   DKIM          on (the only setting). The SMTP destination refuses a message without a signer
#                 ("no key, no connection": smtp/mod.rs), so DKIM cannot be switched off; DKIM=off
#                 exits 3 after saying so, and every result includes the signing cost.
#   RUN_TIMEOUT_S 3600
#
# Results: $M6_OUT/<LABEL>/ (report.json, report.txt, arrivals.ndjson, enrollments.csv,
# metrics-w*.txt, docker-stats.ndjson, db-cpu.json, messages.txt, variant.txt).
#
# Usage: PEOPLE=20000 WORKERS=3 TEMPLATE=html scripts/bench/m6/run.sh LABEL

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

label="${1:-}"
[ -n "$label" ] || { sed -n '2,/^# Usage/p' "$0" >&2; exit 2; }
people="${PEOPLE:-5000}"
workers="${WORKERS:-1}"
template="${TEMPLATE:-blocks}"
dest_rate="${DEST_RATE:-1000}"
limits="${LIMITS:-off}"
timeout_s="${RUN_TIMEOUT_S:-3600}"
export BENCH_WORKER_CONCURRENCY="${WORKER_CONCURRENCY:-4}"
if [ "${DKIM:-on}" != on ]; then
  echo "DKIM=off cannot be run: the SMTP destination always signs and refuses a message with no key (smtp/mod.rs deliver). Nothing was run." >&2
  exit 3
fi
case "$template" in blocks | html) ;; *) die "TEMPLATE is blocks or html" ;; esac
case "$limits" in on | off) ;; *) die "LIMITS is on or off" ;; esac
[ "$workers" -ge 1 ] && [ "$workers" -le 10 ] || die "WORKERS is 1 to 10"
out="$M6_OUT/$label"
[ ! -e "$out" ] || die "$out exists; pick a new label"
mkdir -p "$out"

ws="$(m6_ws)"
dest="$(state_get M6_DEST)"
tpl_key=M6_TPL_HTML
[ "$template" != blocks ] || tpl_key=M6_TPL_BLOCKS
tpl="$(state_get "$tpl_key")"
[ -n "$dest" ] && [ -n "$tpl" ] || die "run setup.sh first"
login || die "cannot sign in; run stack.sh up"
[ -f "$M6_SINK_DIR/sink.pid" ] && kill -0 "$(cat "$M6_SINK_DIR/sink.pid")" 2>/dev/null || die "the sink is not running; sink.sh start"

# 1. Workers: fresh processes, so the metrics cover this run only.
log "starting $workers worker process(es), concurrency $BENCH_WORKER_CONCURRENCY"
compose up -d --no-build --no-deps --force-recreate --scale "zirv-flow-worker=$workers" zirv-flow-worker >&2
ports=()
for i in $(seq 1 "$workers"); do
  hostport="$(compose port --index "$i" zirv-flow-worker 9100 | sed 's/.*://')"
  ports+=("$hostport")
  for _ in $(seq 1 60); do curl -fsS "http://127.0.0.1:$hostport/metrics" >/dev/null 2>&1 && break; sleep 1; done
  curl -fsS "http://127.0.0.1:$hostport/metrics" >/dev/null 2>&1 || die "worker $i has no /metrics on $hostport"
done

# 2. The variant.
settings="$(api GET "/workspaces/$ws/sending/settings")"
revision="$(jq -r .revision <<<"$settings")"
if [ "$limits" = on ]; then
  weekday="$(date -u -d '+3 days' +%a | tr 'A-Z' 'a-z')"
  body="$(jq -nc --argjson r "$revision" --arg d "$weekday" \
    '{frequency_caps: [{channel: "email", window: "P1D", limit: 1000000, mode: "skip"}],
      quiet_hours: {start: "03:00", end: "04:00", days: [$d]}, revision: $r}')"
else
  body="$(jq -nc --argjson r "$revision" '{frequency_caps: [], quiet_hours: null, revision: $r}')"
fi
api_json PUT "/workspaces/$ws/sending/settings" "$body" >/dev/null
api_json PATCH "/workspaces/$ws/destinations/$dest" "$(jq -nc --argjson r "$dest_rate" \
  --rawfile ca "$M6_SINK_DIR/ca.pem" '{rate: $r, burst: $r, config: {ca_certificate: $ca}}')" >/dev/null
{
  echo "label=$label people=$people workers=$workers worker_concurrency=$BENCH_WORKER_CONCURRENCY"
  echo "template=$template dest_rate=$dest_rate limits=$limits dkim=on"
  echo "image=$BENCH_IMAGE_TAG render_pool=${BENCH_RENDER_POOL:-2} tier=$M4_TIER sink_delay_ms=${M6_SINK_DELAY_MS:-0}"
  echo "started=$(date -u +%FT%TZ)"
} >"$out/variant.txt"

# 3. The list and the flow.
list="$(api_json POST "/workspaces/$ws/lists" "$(jq -nc --arg n "bench-$label" '{name: $n}')" | jq -r .id)"
[ -n "$list" ] && [ "$list" != null ] || die "the list was not created"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
pgc -At -v ws="$ws" -v pat="sb-%@$M6_RCPT_DOMAIN" -v n="$people" -c \
  "SELECT person_id FROM identities WHERE workspace_id = :'ws' AND kind = 'email' AND value LIKE :'pat' ORDER BY value LIMIT :n" \
  | split -l 1000 - "$tmp/ids-"
total=0
for part in "$tmp"/ids-*; do
  ids="$(jq -Rsc 'split("\n") | map(select(length > 0))' "$part")"
  api_json POST "/workspaces/$ws/lists/$list/members" "$(jq -nc --argjson p "$ids" '{person_ids: $p}')" >/dev/null
  total=$((total + $(jq length <<<"$ids")))
done
[ "$total" -eq "$people" ] || die "only $total of $people people exist; run seed.sh with SEED_PEOPLE=$people"
graph="$(jq -nc --arg t "$tpl" --arg d "$dest" '{v: 1,
  nodes: [{id: "send", type: "action.send", template: $t, destination: $d}, {id: "end", type: "exit"}],
  edges: [{id: "send-end", from: "send", port: "next", to: "end"}]}')"
flow="$(api_json POST "/workspaces/$ws/flows" "$(jq -nc --arg n "bench-$label" --argjson g "$graph" \
  '{name: $n, graph: $g, settings: {trigger: {type: "manual"}, reentry: {mode: "once"}}}')" | jq -r .id)"
[ -n "$flow" ] && [ "$flow" != null ] || die "the flow was not created"
api POST "/workspaces/$ws/flows/$flow/publish" >/dev/null

# 4. Sampler, start, wait.
sampler() {
  while [ ! -e "$out/stop" ]; do
    # shellcheck disable=SC2046
    docker stats --no-stream --format '{{json .}}' $(compose ps -q) 2>/dev/null \
      | jq -c --arg t "$(date -u +%FT%T.%6NZ)" '. + {t: $t}' >>"$out/docker-stats.ndjson" || true
    sleep 3
  done
}
sampler &
sampler_pid=$!
count_since() { awk -F'[:,]' -v t="$1" '$2 + 0 >= t' "$M6_SINK_DIR/arrivals.ndjson" | wc -l; }

t0="$(date +%s%6N)"
api_json POST "/workspaces/$ws/flows/$flow/start" "$(jq -nc --arg l "$list" '{list_id: $l}')" >/dev/null
log "run $label started: $people people, flow $flow"
last=-1
stalled_since="$(date +%s)"
deadline=$(( $(date +%s) + timeout_s ))
while :; do
  got="$(count_since "$t0")"
  [ "$got" -lt "$people" ] || break
  now="$(date +%s)"
  if [ "$got" != "$last" ]; then last="$got"; stalled_since="$now"; fi
  [ $((now - stalled_since)) -lt 180 ] || { log "stalled at $got of $people for 180 s"; break; }
  [ "$now" -lt "$deadline" ] || { log "timeout at $got of $people"; break; }
  sleep 5
done
sleep 2
touch "$out/stop"
wait "$sampler_pid" 2>/dev/null || true

# 5. Collect.
awk -F'[:,]' -v t="$t0" '$2 + 0 >= t' "$M6_SINK_DIR/arrivals.ndjson" >"$out/arrivals.ndjson"
for i in "${!ports[@]}"; do
  curl -fsS "http://127.0.0.1:${ports[$i]}/metrics" >"$out/metrics-w$((i + 1)).txt" || true
done
pgc -At -v flow="$flow" -c \
  "SELECT i.value || ',' || (extract(epoch FROM e.created_at) * 1000000)::bigint
     FROM enrollments e JOIN identities i ON i.person_id = e.person_id AND i.kind = 'email'
    WHERE e.flow_id = :'flow'" >"$out/enrollments.csv"
pgc -At -v flow="$flow" -c \
  "SELECT 'messages.' || status || coalesce('/' || reason, '') || ' ' || n FROM (SELECT status, reason, count(*) AS n FROM messages WHERE flow_id = :'flow' GROUP BY status, reason) t ORDER BY 1" >"$out/messages.txt"
pgc -At -c "SELECT 'jobs.' || kind || '.' || state || ' ' || n FROM (SELECT kind, state, count(*) AS n FROM jobs WHERE state IN ('dead', 'ready', 'running') GROUP BY kind, state) t ORDER BY 1" >>"$out/messages.txt"
# Database CPU: the postgres container's samples, percent of one core.
pgname="$(docker inspect --format '{{.Name}}' "$(pg_container)" | sed 's|^/||')"
jq -sc --arg n "$pgname" '[.[] | select(.Name == $n) | (.CPUPerc | rtrimstr("%") | tonumber)]
  | {samples: length, mean_pct: (if length > 0 then add / length else null end), max_pct: (max // null)}' \
  "$out/docker-stats.ndjson" >"$out/db-cpu.json"
{
  echo "finished=$(date -u +%FT%TZ)"
  echo "db_cpu=$(cat "$out/db-cpu.json")"
} >>"$out/variant.txt"

# 6. Report. A run held to the target by the bucket (DEST_RATE at most 300) gets a 3% tolerance.
tol=0
[ "$dest_rate" -gt 300 ] || tol=0.03
metrics_args=()
for f in "$out"/metrics-w*.txt; do metrics_args+=(--metrics "$f"); done
"$SENDBENCH" report --arrivals "$out/arrivals.ndjson" --enrollments "$out/enrollments.csv" \
  "${metrics_args[@]}" --label "$label" --target 300 --tolerance "$tol" --expected "$people" \
  --out "$out/report.json" | tee "$out/report.txt"
jq -c --slurpfile c "$out/db-cpu.json" '.metrics + {db_cpu: $c[0]}' "$out/report.json" >&2
