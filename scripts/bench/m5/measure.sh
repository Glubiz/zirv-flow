#!/usr/bin/env bash
# The M5 timer-lateness measurement (issue #169; architecture 9.3 and 19). A run is: seed.sh
# staged and loaded the waiting enrollments with a window start t0 ahead; `start` waits for t0 and
# runs the sampler (and the load generator for a loaded run); `collect` computes the results.
#
#   start RUN LABEL [load]   waits until no cargo.exe runs, then until t0 - M5_PRE_S, then starts
#                            detached: the sampler (every M5_TICK_S seconds the queue depth, the
#                            job table counters and the oldest due job's age; every 10 s docker
#                            stats) and, with `load`, tools/loadgen at M5_LOAD_RATE track events/s
#                            for the whole window. Returns at once; the work goes on in the
#                            background, logging to $M4_OUT/run<RUN>-<LABEL>/supervisor.log.
#   wait RUN LABEL           blocks (at most M5_WAIT_S seconds, default 3000) until the run has
#                            finished: every seeded wake job is gone and the load generator ended.
#   collect RUN LABEL        stops the sampler, and writes lateness.json (lateness.sql), rates.csv
#                            (queue depth and claim, completion rates per sampling interval),
#                            docker-stats-summary.json and, for a loaded run, loadgen.json to
#                            $M4_OUT/run<RUN>-<LABEL>/, plus summary.json with the headline figures.
#   status RUN LABEL         where the run stands
#
# Lateness = the start of the step after the wait (step_executions.started_at of node x1, the
# transaction start of the wake job) minus the seeded wake instant. Claim rate = the sampler's
# deltas of pg_stat_user_tables.n_tup_upd on jobs (one update per claim, so an upper bound that
# also counts lease renewals); the completion rate is the rate of deleted jobs.
#
# Environment (see common.sh for the rest):
#   M5_TICK_S      sampler interval, default 5
#   M5_PRE_S       seconds before t0 at which the sampler starts, default 60
#   M5_LOAD_RATE   loadgen track events/s, default 2000
#   M5_LOAD_POOL   loadgen --people, default 100000
#   M5_LOAD_SEED   loadgen --seed, default 51 (pass a new one for each loaded run)
#   LOADGEN        the loadgen binary, default <repo>/target/release/loadgen
#
# Usage: scripts/bench/m5/measure.sh start|wait|collect|status RUN LABEL [load]

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

cmd="${1:-}"
run="${2:-}"
label="${3:-}"
mode="${4:-}"
[ -n "$cmd" ] || { sed -n '2,/^# Usage/p' "$0" >&2; exit 2; }
if [ "$cmd" = start ] && [ "$label" = load ] && [ -z "$mode" ]; then
  echo "measure.sh: a label of 'load' with no mode would run without load; use: start RUN LABEL load (for example: start 3 loaded load)" >&2
  exit 2
fi
tick="${M5_TICK_S:-5}"
pre="${M5_PRE_S:-60}"
rate="${M5_LOAD_RATE:-2000}"
pool="${M5_LOAD_POOL:-100000}"
seed="${M5_LOAD_SEED:-51}"
loadgen="${LOADGEN:-$root/target/release/loadgen}"
out="$M4_OUT/run${run}-${label}"

t0_of() { pgc -At -c "SELECT to_char(t0 AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM m5bench.runs WHERE run = $run"; }
window_of() { pgc -At -c "SELECT window_s FROM m5bench.runs WHERE run = $run"; }
epoch_of() { date -u -d "$1" +%s; }
jobs_left() { pgc -At -c "SELECT count(*) FROM jobs j JOIN m5bench.rows r ON r.job_id = j.id WHERE r.run = $run"; }

# One JSON line per tick: the queue and the job table counters.
sample_sql() {
  cat <<'SQL'
SELECT json_build_object(
  't', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'due_ready', (SELECT count(*) FROM jobs WHERE state = 'ready' AND run_at <= clock_timestamp()),
  'running', (SELECT count(*) FROM jobs WHERE state = 'running'),
  'oldest_due_age_s', (SELECT extract(epoch FROM clock_timestamp() - min(run_at)) FROM jobs WHERE state = 'ready' AND run_at <= clock_timestamp()),
  'jobs_upd', (SELECT n_tup_upd FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'jobs_del', (SELECT n_tup_del FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'jobs_ins', (SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'steps_ins', (SELECT n_tup_ins FROM pg_stat_user_tables WHERE relname = 'step_executions'),
  'jobs_dead_tup', (SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = 'jobs'),
  'db_conns', (SELECT count(*) FROM pg_stat_activity WHERE datname = 'zirv_flow'));
SQL
}

sampler_loop() { # runs detached
  local next_docker=0 now
  while [ ! -e "$out/stop" ]; do
    sample_sql | pgc -At -f - >>"$out/samples.ndjson" 2>>"$out/sampler.err" || true
    now="$(date +%s)"
    if [ "$now" -ge "$next_docker" ]; then
      next_docker=$((now + 10))
      # shellcheck disable=SC2046
      docker stats --no-stream --format '{{json .}}' $(compose ps -q) 2>>"$out/sampler.err" \
        | jq -c --arg t "$(date -u +%FT%T.%6NZ)" '. + {t: $t}' >>"$out/docker-stats.ndjson" || true
    fi
    { printf "%s " "$(date -u +%FT%T.%6NZ)"; tasklist //FI "IMAGENAME eq cargo.exe" 2>/dev/null | grep -ci cargo.exe || true; } >>"$out/cargo.log"
    sleep "$tick"
  done
}

supervise() { # runs detached: waits for the window, then starts the sampler and the load
  local t0 t0e window now
  t0="$(t0_of)"; window="$(window_of)"
  [ -n "$t0" ] || die "run $run is not staged"
  t0e="$(epoch_of "$t0")"
  log "run $run $label: window opens $t0 (spread $window s); waiting for cargo.exe to end"
  wait_no_cargo
  while [ "$(date +%s)" -lt $((t0e - pre)) ]; do sleep 5; done
  wait_no_cargo
  log "sampler starts"
  sampler_loop &
  echo "$!" >"$out/sampler.pid"
  if [ "$mode" = load ]; then
    while [ "$(date +%s)" -lt "$t0e" ]; do sleep 0.2; done
    log "loadgen starts: $rate track events/s for $window s over $pool people, seed $seed"
    "$loadgen" --url "$api_base" --key-file "$M4_STATE/loadgen.key" --mode track --rate "$rate" \
      --duration "$window" --people "$pool" --seed "$seed" --run-id "m5-run${run}-$(date -u +%H%M%S)" \
      --out "$out/loadgen.json" >"$out/loadgen.log" 2>&1 || log "loadgen exited non-zero"
    log "loadgen ended"
  fi
  until [ "$(jobs_left)" = 0 ]; do
    now="$(date +%s)"
    [ "$now" -lt $((t0e + window + 3600)) ] || { log "timeout: $(jobs_left) wake jobs left"; break; }
    sleep 10
  done
  sleep "$((tick * 2))"
  log "run finished: $(jobs_left) wake jobs left"
  : >"$out/finished"
}

case "$cmd" in
  start)
    [ -n "$run" ] && [ -n "$label" ] || die "usage: measure.sh start RUN LABEL [load]"
    [ "$mode" != load ] || [ -x "$loadgen" ] || die "no loadgen at $loadgen"
    [ "$mode" != load ] || [ -s "$M4_STATE/loadgen.key" ] || die "no ingest key"
    mkdir -p "$out"
    rm -f "$out/stop" "$out/finished"
    log "starting the supervisor; its log is $out/supervisor.log"
    nohup "$0" _supervise "$run" "$label" "$mode" >>"$out/supervisor.log" 2>&1 &
    echo "$!" >"$out/supervisor.pid"
    ;;
  _supervise) supervise ;;
  wait)
    deadline=$(( $(date +%s) + ${M5_WAIT_S:-3000} ))
    until [ -e "$out/finished" ]; do
      [ "$(date +%s)" -lt "$deadline" ] || { log "not finished yet"; exit 1; }
      sleep 15
    done
    log "run $run $label finished"
    ;;
  status)
    t0="$(t0_of)"
    echo "t0=$t0 now=$(date -u +%FT%TZ) jobs_left=$(jobs_left) finished=$([ -e "$out/finished" ] && echo yes || echo no)"
    tail -n 3 "$out/supervisor.log" 2>/dev/null || true
    ;;
  collect)
    : >"$out/stop"
    sleep "$((tick + 3))"
    pgc -At -v run="$run" -f - <"$m5_dir/lateness.sql" | jq . >"$out/lateness.json"
    # Rates per sampling interval, from the cumulative counters.
    jq -rs '
      def secs: sub("\\.[0-9]+Z$"; "Z") | fromdate;
      ["t","due_ready","running","oldest_due_age_s","claims_per_s","completions_per_s","steps_per_s"],
      ([., .[1:]] | transpose[:-1][] | . as [$a, $b]
        | (($b.t | secs) - ($a.t | secs)) as $dt
        | select($dt > 0)
        | [$b.t, $b.due_ready, $b.running, $b.oldest_due_age_s,
           (($b.jobs_upd - $a.jobs_upd) / $dt), (($b.jobs_del - $a.jobs_del) / $dt),
           (($b.steps_ins - $a.steps_ins) / $dt)]) | @csv' "$out/samples.ndjson" >"$out/rates.csv"
    jq -s '
      group_by(.Name) | map({container: .[0].Name,
        samples: length,
        cpu_pct_avg: (map(.CPUPerc | rtrimstr("%") | tonumber) | add / length),
        cpu_pct_max: (map(.CPUPerc | rtrimstr("%") | tonumber) | max),
        mem_usage_last: .[-1].MemUsage})' "$out/docker-stats.ndjson" >"$out/docker-stats-summary.json"
    # Headline figures: the busiest minute of the claim rate, the deepest queue.
    jq -n --slurpfile l "$out/lateness.json" --arg label "$label" --arg run "$run" \
      --rawfile rates "$out/rates.csv" --slurpfile d "$out/docker-stats-summary.json" '
      ($rates | split("\n")[1:] | map(select(length > 0) | split(",") | map(gsub("\""; "")))) as $rows
      | {run: ($run | tonumber), label: $label, lateness: $l[0].sets,
         overall: $l[0].overall, spread_in_burst_minute: $l[0].spread_in_burst_minute,
         spread_outside_burst_minute: $l[0].spread_outside_burst_minute,
         dead_wake_jobs: $l[0].dead_wake_jobs, jobs_left: $l[0].jobs_left,
         max_due_ready: ($rows | map(.[1] | tonumber) | max),
         max_oldest_due_age_s: ($rows | map(.[3] | select(. != "") | tonumber) | max),
         claims_per_s_peak: ($rows | map(.[4] | tonumber) | max),
         completions_per_s_peak: ($rows | map(.[5] | tonumber) | max),
         docker: $d[0]}' >"$out/summary.json"
    if [ -s "$out/loadgen.json" ]; then
      jq -c '{achieved_rate: .total.achieved_rate, requests: .total.requests, status: .total.status,
              latency_from_schedule: .total.latency_from_schedule, service_latency: .total.service_latency,
              dispatch_lag: .total.dispatch_lag}' "$out/loadgen.json" >"$out/loadgen-summary.json"
    fi
    jq -c '{run, label, lateness, overall, spread_in_burst_minute, dead_wake_jobs, jobs_left, max_due_ready, claims_per_s_peak}' "$out/summary.json"
    ;;
  *) die "unknown command $cmd" ;;
esac
