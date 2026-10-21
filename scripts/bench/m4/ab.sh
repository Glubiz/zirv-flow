#!/usr/bin/env bash
# The factor A/B of issue #151 (ingest API p99 with and without segment re-evaluation), on the
# M4 bench stack. One run per call; alternate the conditions A B A B by hand with a fresh seed.
#
#   A  a fresh agg90 segment is saved and refreshed, then loadgen sends track events: the
#      re-evaluation of that segment runs during the load (the `incremental` phase of measure.sh)
#   B  every segment of the workspace is archived, then the same loadgen run: events still mark
#      people and enqueue a `segment.reevaluate` job (the store does that for every batch), but
#      the job finds no active segment to evaluate
#
# Before the window the inbox, the dirty marks and every job that is due must be drained. During
# it a sampler records, every few seconds, `docker stats` of the stack and the job queue per kind
# and state. After it the script writes $M4_OUT/ab-<cond>-<seed>.json (and the raw samples next
# to it) with the loadgen latencies (server-side service_latency, latency_from_schedule, dispatch
# lag), the CPU summaries, the queue depths and the counters of the window.
#
# Usage: scripts/bench/m4/ab.sh A|B SEED
# Environment: see common.sh; M4_INC_RATE (2000), M4_INC_SECONDS (600), M4_INC_POOL (1000000),
# M4_STATS_EVERY (3 s), M4_DRAIN_TIMEOUT (900 s), LOADGEN.

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

cond="${1:-}"
seed="${2:-}"
case "$cond" in A | B) ;; *) echo "usage: ab.sh A|B SEED" >&2; exit 2 ;; esac
[ -n "$seed" ] || { echo "usage: ab.sh A|B SEED" >&2; exit 2; }

rate="${M4_INC_RATE:-2000}"
seconds="${M4_INC_SECONDS:-600}"
pool="${M4_INC_POOL:-1000000}"
stats_every="${M4_STATS_EVERY:-3}"
drain_timeout="${M4_DRAIN_TIMEOUT:-900}"
loadgen="${LOADGEN:-$root/target/release/loadgen}"
ws="$(state_get M4_WS)"
[ -n "$ws" ] || die "no workspace: run setup.sh and seed.sh first"
[ -x "$loadgen" ] || die "no loadgen at $loadgen"
[ -s "$M4_STATE/loadgen.key" ] || die "no ingest key; run setup.sh"
login || die "cannot sign in; run stack.sh up"

tag="$cond-$seed"
prefix="$M4_OUT/ab-$tag"
sampler_pids=()
cleanup() { for p in "${sampler_pids[@]:-}"; do [ -z "$p" ] || kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

# Dirty marks, inbox rows and re-evaluation or other jobs that are due now (the flow wake-ups
# the M5 bench left are dated in the future and are not counted).
outstanding() {
  pg -At -v ws="$ws" -f - <<'SQL'
SELECT (SELECT count(*) FROM person_changes WHERE workspace_id = :'ws')
     + (SELECT count(*) FROM ingest_inbox)
     + (SELECT count(*) FROM jobs WHERE run_at <= now() AND state IN ('ready', 'running'));
SQL
}

wait_drained() {
  local deadline=$(( $(date +%s) + drain_timeout )) n quiet=0
  while :; do
    n="$(outstanding)"
    if [ "$n" = 0 ]; then quiet=$((quiet + 1)); else quiet=0; fi
    [ "$quiet" -ge 3 ] && return 0
    [ "$(date +%s)" -lt "$deadline" ] || die "not drained after ${drain_timeout}s: $n outstanding"
    sleep 2
  done
}

wait_no_cargo() {
  until ! tasklist //FI "IMAGENAME eq cargo.exe" 2>/dev/null | grep -qi cargo.exe; do sleep 30; done
}

active_segments() {
  pg -At -v ws="$ws" -f - <<'SQL'
SELECT id FROM segments WHERE workspace_id = :'ws' AND archived_at IS NULL;
SQL
}

counters() { # a JSON object of the cumulative counters the window is diffed on
  pg -At -v ws="$ws" -f - <<'SQL'
SELECT json_build_object(
  'at', clock_timestamp(),
  'jobs_ins', (SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'jobs_del', (SELECT n_tup_del FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'jobs_upd', (SELECT n_tup_upd FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'changes_ins', (SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname = 'person_changes'),
  'changes_upd', (SELECT n_tup_upd FROM pg_stat_user_tables WHERE relname = 'person_changes'),
  'members_ins', (SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname = 'segment_members'),
  'members_upd', (SELECT n_tup_upd FROM pg_stat_user_tables WHERE relname = 'segment_members'),
  'decisions', (SELECT count(*) FROM decision_records WHERE workspace_id = :'ws'
                AND recorded_at >= now() - interval '1 day'),
  'xact_commit', (SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()),
  'wal_bytes', (SELECT wal_bytes FROM pg_stat_wal),
  'checkpoints', (SELECT checkpoints_timed + checkpoints_req FROM pg_stat_bgwriter));
SQL
}

# The sampler of docker stats: one CSV line per container per sample.
containers="$(compose ps -q | tr '\n' ' ')"
stats_loop() {
  while :; do
    # shellcheck disable=SC2086
    docker stats --no-stream --format "$(date -u +%T),{{.Name}},{{.CPUPerc}},{{.MemUsage}}" $containers 2>/dev/null \
      | sed -E 's/\/ [0-9.]+[A-Za-z]+//; s/%//' || true
    sleep "$stats_every"
  done
}
# The queue per kind and state, and the unclaimed marks and inbox rows, every 2 s, over one
# long-lived psql so that the sample itself is cheap.
queue_loop() {
  while :; do
    pg -At -F, -c "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'HH24:MI:SS'), coalesce(kind, 'none'), coalesce(state, 'none'), count(*)
      FROM (SELECT kind, state FROM jobs WHERE run_at <= now() UNION ALL SELECT NULL, NULL) j GROUP BY 2, 3
      UNION ALL SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'HH24:MI:SS'), 'person_changes', 'rows', count(*) FROM person_changes
      UNION ALL SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'HH24:MI:SS'), 'ingest_inbox', 'rows', count(*) FROM ingest_inbox" 2>/dev/null || true
    sleep 2
  done
}

log "$tag: waiting for no cargo.exe, then draining"
wait_no_cargo
wait_drained

seg=""
if [ "$cond" = A ]; then
  name="m4-ab-$tag"
  t_call="$(pg -At -c 'SELECT clock_timestamp()')"
  seg="$(api_json POST "/workspaces/$ws/segments" \
    "$(jq -nc --arg n "$name" --slurpfile d "$m4_dir/defs/agg90.json" '{name: $n, definition: $d[0], cadence_minutes: 10080}')" | jq -r .id)"
  # The first refresh: wait until the segment is computed.
  until [ "$(api GET "/workspaces/$ws/segments/$seg" | jq -r '.computed_version // 0')" != 0 ]; do sleep 1; done
  log "$tag: segment $seg refreshed ($(api GET "/workspaces/$ws/segments/$seg" | jq -c '{member_count, computed_version}'))"
  # Other active segments (none expected) would add re-evaluation work to this condition.
  for other in $(active_segments); do [ "$other" = "$seg" ] || die "another active segment: $other"; done
else
  for other in $(active_segments); do
    log "$tag: archiving segment $other"
    api POST "/workspaces/$ws/segments/$other/archive" >/dev/null
  done
  [ -z "$(active_segments)" ] || die "an active segment remains"
fi
wait_drained
wait_no_cargo

active_n="$(active_segments | grep -c . || true)"
counters >"$prefix.counters-before.json"
since="$(pg -At -c "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")"
: >"$prefix.docker-stats.csv"
: >"$prefix.queue.csv"
stats_loop >>"$prefix.docker-stats.csv" &
sampler_pids+=("$!")
queue_loop >>"$prefix.queue.csv" &
sampler_pids+=("$!")

log "$tag: loadgen $rate/s for $seconds s over $pool people, seed $seed, $active_n active segment(s)"
"$loadgen" --url "$api_base" --key-file "$M4_STATE/loadgen.key" --mode track --rate "$rate" \
  --duration "$seconds" --people "$pool" --seed "$seed" --run-id "ab-$tag" \
  --out "$prefix.loadgen.json" >&2
window_end="$(pg -At -c 'SELECT clock_timestamp()')"
counters >"$prefix.counters-end-of-load.json"
log "$tag: loadgen done; draining"
cleanup
sampler_pids=()
wait_drained || true
counters >"$prefix.counters-after.json"

latency=null
members_after=null
if [ "$cond" = A ]; then
  latency="$(pg -At -v ws="$ws" -v segment="$seg" -v since="$since" -f - <"$m4_dir/incremental-latency.sql")"
  members_after="$(api GET "/workspaces/$ws/segments/$seg" | jq -c '{member_count, computed_version}')"
  api POST "/workspaces/$ws/segments/$seg/archive" >/dev/null
fi

# Summaries: CPU (percent of one core, as docker reports it) per container, and the queue.
cpu="$(awk -F, '{ n[$2]++; s[$2] += $3; if ($3 > m[$2]) m[$2] = $3 }
  END { for (c in n) printf "%s,%d,%.1f,%.1f\n", c, n[c], s[c] / n[c], m[c] }' "$prefix.docker-stats.csv" \
  | jq -R 'split(",") | {container: .[0], samples: (.[1] | tonumber), mean_cpu_pct: (.[2] | tonumber), max_cpu_pct: (.[3] | tonumber)}' | jq -sc .)"
queue="$(awk -F, '{ k = $2 "/" $3; n[k]++; s[k] += $4; if ($4 > m[k]) m[k] = $4 }
  END { for (k in n) printf "%s,%d,%.2f,%d\n", k, n[k], s[k] / n[k], m[k] }' "$prefix.queue.csv" \
  | jq -R 'split(",") | {series: .[0], samples: (.[1] | tonumber), mean: (.[2] | tonumber), max: (.[3] | tonumber)}' | jq -sc .)"
# Fraction of the queue samples (taken while loadgen ran) that saw a running re-evaluation job.
reeval_running="$(awk -F, '$2 == "segment.reevaluate" && $3 == "running" { t[$1] = 1 } { all[$1] = 1 }
  END { a = 0; r = 0; for (x in all) a++; for (x in t) r++; printf "{\"seconds_sampled\": %d, \"seconds_with_running_reevaluate\": %d}", a, r }' "$prefix.queue.csv")"

jq -n --arg cond "$cond" --argjson seed "$seed" --arg ws "$ws" --arg seg "$seg" --arg since "$since" \
  --arg end "$window_end" --argjson active "$active_n" --argjson rate "$rate" --argjson seconds "$seconds" \
  --argjson pool "$pool" --argjson latency "$latency" --argjson members_after "$members_after" \
  --argjson cpu "$cpu" --argjson queue "$queue" --argjson reeval "$reeval_running" \
  --arg image "$BENCH_IMAGE_TAG" --arg at "$(date -u +%FT%TZ)" \
  --slurpfile lg "$prefix.loadgen.json" --slurpfile b "$prefix.counters-before.json" \
  --slurpfile e "$prefix.counters-end-of-load.json" --slurpfile a "$prefix.counters-after.json" '
  def diff($x; $y): reduce ($y | keys[] | select(. != "at")) as $k ({}; . + {($k): ($y[$k] - $x[$k])});
  {condition: $cond, seed: $seed, image: $image, workspace: $ws, segment: (if $seg == "" then null else $seg end),
   active_segments_at_start: $active, measured_at: $at, window_start: $since, window_end: $end,
   loadgen_cfg: {rate_per_s: $rate, seconds: $seconds, people: $pool},
   loadgen: {requests: $lg[0].total.requests, accepted: $lg[0].total.items_accepted, achieved_rate: $lg[0].total.achieved_rate,
             status: $lg[0].total.status,
             service_latency_ms: $lg[0].total.service_latency,
             latency_from_schedule_ms: $lg[0].total.latency_from_schedule,
             dispatch_lag_ms: $lg[0].total.dispatch_lag},
   counters_during_load: diff($b[0]; $e[0]),
   counters_including_drain: diff($b[0]; $a[0]),
   reevaluate_running: $reeval,
   incremental_latency_s: $latency, members_after: $members_after,
   docker_stats_cpu: $cpu, queue_depth: $queue}' >"$prefix.json"
jq -c '{condition, seed, active_segments_at_start, p99_service: .loadgen.service_latency_ms.p99_ms,
  p99_schedule: .loadgen.latency_from_schedule_ms.p99_ms, lag_p99: .loadgen.dispatch_lag_ms.p99_ms,
  jobs_ins: .counters_during_load.jobs_ins, reevaluate_running}' "$prefix.json"
