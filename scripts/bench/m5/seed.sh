#!/usr/bin/env bash
# Seeds the waiting enrollments of the M5 timer benchmark (issue #169) by SQL, mirroring the rows
# the runtime itself writes (supervisor ruling 46fb269a). This measures timers only: it gives no
# evidence of enrollment throughput.
#
# The shape comes from the sample sample.sh enrolled through the product path on the published
# image (the oldest sample enrollment of m5-spread and of m5-burst, with its step_executions row,
# its flow.step wake job, its flow_entries row and its three decision_records). Per seeded
# enrollment the script writes one row to each of those five tables, copying every column of the
# sample row and changing only the ids, the person, the trace and the instants:
#
#   enrollments      state waiting, node w1, state_version 1, wake_at = the wake instant
#   step_executions  (enrollment, w1), outcome waiting
#   jobs             flow.step, input wake, ready, run_at = wake_at, unique key wake:<id>:1
#   flow_entries     entries 1
#   decision_records flow.enroll/enrolled, flow.step/waiting, flow.wait/waiting
#
# Sets (the workspace's first 100 people are the sample and never used):
#   spread  SEED_SPREAD (default 900000) enrollments of m5-spread, wake_at uniform over the window
#           [t0, t0 + SEED_WINDOW_S)
#   burst   SEED_BURST (default 100000) enrollments of m5-burst, all waking in the minute
#           [t0 + SEED_BURST_AT_S, +60 s)
#
# Steps (SEED_STEPS, default "stage load vacuum verify"), each short enough for one command:
#   stage   write m5bench.tmpl, .rows and .runs for RUN (no product table is touched)
#   load    insert the product rows in chunks of SEED_CHUNK (default 100000) per transaction
#   vacuum  VACUUM (ANALYZE) the touched tables, then CHECKPOINT
#   verify  verify.sql: the counts and the shape against the sample; fails on any difference
#   clean   delete this run's seeded rows (product tables and staging), so the next run starts
#           from the same table sizes; nothing else is deleted
#
# Environment (see common.sh for the rest):
#   RUN               the run number, default 1
#   M5_T0_LEAD_MIN    minutes from now to the window start, default 70: the seed must finish
#                     before it (stage logs t0; verify refuses to pass when t0 is less than
#                     SEED_MIN_MARGIN_S, default 120, away)
#   SEED_WINDOW_S     the spread, default 2400 (40 min: 375 wakes/s)
#   SEED_BURST_AT_S   the burst's offset in the window, default 1200
#
# Usage: RUN=1 scripts/bench/m5/seed.sh [step ...]

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

run="${RUN:-1}"
n_spread="${SEED_SPREAD:-900000}"
n_burst="${SEED_BURST:-100000}"
window_s="${SEED_WINDOW_S:-2400}"
burst_at_s="${SEED_BURST_AT_S:-1200}"
chunk="${SEED_CHUNK:-100000}"
lead="${M5_T0_LEAD_MIN:-70}"
margin="${SEED_MIN_MARGIN_S:-120}"
ws="$(state_get M4_WS)"
spread_flow="$(state_get M5_FLOW_SPREAD)"
burst_flow="$(state_get M5_FLOW_BURST)"
[ -n "$ws" ] && [ -n "$spread_flow" ] && [ -n "$burst_flow" ] || die "run setup.sh and sample.sh first"
case "$run$n_spread$n_burst$window_s$burst_at_s$chunk$lead" in *[!0-9]* | "") die "numeric settings only" ;; esac

steps=("$@")
[ "${#steps[@]}" -gt 0 ] || steps=(stage load vacuum verify)

stage() {
  local t0
  t0="$(pgc -At -c "SELECT to_char(now() + $lead * interval '1 minute', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")"
  log "stage run $run: window opens at $t0 ($lead min from now), $n_spread spread over $window_s s, $n_burst burst at +$burst_at_s s"
  pgc -v ws="$ws" -v spread_flow="$spread_flow" -v burst_flow="$burst_flow" -v run="$run" -v t0="$t0" \
    -v window_s="$window_s" -v burst_at_s="$burst_at_s" -v n_spread="$n_spread" -v n_burst="$n_burst" \
    -f - <"$m5_dir/seed-stage.sql"
}

load() {
  local set total lo hi started
  started="$(date +%s)"
  for set in spread burst; do
    if [ "$set" = spread ]; then total="$n_spread"; else total="$n_burst"; fi
    for ((lo = 1; lo <= total; lo += chunk)); do
      hi=$((lo + chunk - 1)); [ "$hi" -le "$total" ] || hi="$total"
      pgc -v run="$run" -v set="$set" -v lo="$lo" -v hi="$hi" -f - <"$m5_dir/seed-load.sql"
      log "loaded $set $hi of $total ($(( $(date +%s) - started )) s since the load began)"
    done
  done
}

vacuum() {
  local t
  for t in enrollments step_executions jobs flow_entries decision_records; do
    log "vacuum (analyze, parallel 0: a parallel vacuum needs more /dev/shm than the stack gives) $t"
    pgc -c "VACUUM (ANALYZE, PARALLEL 0) $t"
  done
  pgc -c "CHECKPOINT"
}

verify() {
  pgc -v ws="$ws" -v spread_flow="$spread_flow" -v burst_flow="$burst_flow" -v run="$run" \
    -v n_spread="$n_spread" -v n_burst="$n_burst" -v margin="$margin" \
    -At -f - <"$m5_dir/verify.sql" | tee "$M4_OUT/seed-verify-run$run.txt"
  if grep -q '^FAIL' "$M4_OUT/seed-verify-run$run.txt"; then die "seed validation failed"; fi
}

clean() {
  local set total lo hi
  for set in spread burst; do
    if [ "$set" = spread ]; then total="$n_spread"; else total="$n_burst"; fi
    for ((lo = 1; lo <= total; lo += chunk)); do
      hi=$((lo + chunk - 1)); [ "$hi" -le "$total" ] || hi="$total"
      pgc -v run="$run" -v set="$set" -v lo="$lo" -v hi="$hi" -v ws="$ws" -f - <<'SQL'
BEGIN;
DELETE FROM decision_records d USING m5bench.rows r
 WHERE d.workspace_id = :'ws' AND d.trace_id = r.trace_id
   AND r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;
DELETE FROM jobs j USING m5bench.rows r
 WHERE j.id = r.job_id AND r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;
DELETE FROM flow_entries f USING m5bench.rows r
 WHERE f.person_id = r.person_id AND f.flow_id IN (SELECT id FROM flows WHERE name IN ('m5-spread', 'm5-burst'))
   AND r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;
DELETE FROM enrollments e USING m5bench.rows r
 WHERE e.id = r.enrollment_id AND r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;
COMMIT;
SQL
      log "cleaned $set $hi of $total"
    done
  done
  pgc -v run="$run" -f - <<'SQL'
DELETE FROM m5bench.rows WHERE run = :run;
DELETE FROM m5bench.runs WHERE run = :run;
SQL
}

for step in "${steps[@]}"; do
  case "$step" in
    stage | load | vacuum | verify | clean) "$step" ;;
    *) die "unknown step $step" ;;
  esac
done
