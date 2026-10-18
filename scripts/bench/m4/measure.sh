#!/usr/bin/env bash
# The M4 segment measurements (issue #127, spec D4.1 to D4.7; architecture 8.2, 19, 21). Each
# phase goes through the code path a user hits, the HTTP API as the bench owner (stack.sh up,
# setup.sh and seed.sh first), and writes raw JSON, plus the last EXPLAIN plan where a plan
# applies, to $M4_OUT. Nothing is judged here: the pass or miss calls are for the results doc.
#
#   verify         the preview count of each known definition (defs/*.json) against a
#                  hand-written SQL count over the seeded rows (verify.sql)         verify.json
#   preview-jsonb  D4.1  trait-only preview on jsonb: 1 warm-up, then M4_RUNS timed;
#                  p50 and p95 (nearest rank); target p95 < 3 s                     preview-jsonb.json/.explain.txt
#   index          D4.2  POST .../traits/{key}/index for the definition's traits (plan, age,
#                  marketing_opt_in), one after another, the time of each backfill until
#                  `ready` (a data point, not a target)                             index.json
#   preview-slots  D4.3  the same definition after cutover; EXPLAIN shows the slot indexes
#                                                                                   preview-slots.json/.explain.txt
#   preview-agg    D4.4  count of bench_order_placed >= 2 in the last 90 days with a trait
#                  alongside; target p95 < 15 s                                     preview-agg.json/.explain.txt
#   preview-sum    D4.5  sum of amount where category = books over 90 days (OQ-2's open
#                  case), against the same 15 s                                     preview-sum.json/.explain.txt
#   refresh-trait  D4.6  full refresh of a saved trait-only segment: M4_REFRESH_RUNS fresh
#                  segments, each timed on its first (save) refresh and on a manual
#                  POST .../refresh, from the job's created_at to the job's absence, with
#                  the median; targets < 2 min                                      refresh-trait.json
#   refresh-agg    D4.6  the same for the aggregate segment; target < 10 min        refresh-agg.json
#   incremental    D4.7  with a saved aggregate segment refreshed, loadgen sends track events
#                  (bench_order_placed among them) at M4_INC_RATE/s for M4_INC_SECONDS; once
#                  the marks drain, incremental-latency.sql gives the p95 from an event's
#                  receipt to its segment.entered decision; target p95 < 60 s
#                                                                                   incremental.json, incremental-loadgen.json
#   all            every phase above in this order (one heavy phase at a time, spec D6)
#
# Environment (see common.sh for the rest):
#   M4_RUNS            timed previews per series, default 20 (after 1 warm-up)
#   M4_REFRESH_RUNS    refresh runs per kind, default 5 (the median of 5)
#   M4_INC_RATE        loadgen items/s, default 2000
#   M4_INC_SECONDS     loadgen duration, default 600 (use 60 for a smoke run)
#   M4_INC_POOL        loadgen --people, default 100000; it must not exceed seed.sh's
#                      SEED_LG_POOL, the people that have a `bench-u-N` identity
#   LOADGEN            the loadgen binary, default <repo>/target/release/loadgen
#                      (cargo build --release -p zirv-flow-loadgen)
#   M4_DRAIN_TIMEOUT   seconds to wait for the marks to drain after loadgen, default 900
#
# A preview that runs past the API's statement timeout (10 s, fixed in the shipped build) is a 503
# with code `segment_preview_timeout`; it is recorded as a sample with that status, and the series
# summary counts them in `non_200`.
#
# Usage: scripts/bench/m4/measure.sh <phase> [<phase> ...]

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

runs="${M4_RUNS:-20}"
refresh_runs="${M4_REFRESH_RUNS:-5}"
inc_rate="${M4_INC_RATE:-2000}"
inc_seconds="${M4_INC_SECONDS:-600}"
inc_pool="${M4_INC_POOL:-100000}"
loadgen="${LOADGEN:-$root/target/release/loadgen}"
drain_timeout="${M4_DRAIN_TIMEOUT:-900}"
ws="$(state_get M4_WS)"
[ -n "$ws" ] || die "no workspace: run setup.sh and seed.sh first"
login || die "cannot sign in; run stack.sh up"
trap 'rm -f "${tmp_files[@]:-}"' EXIT
tmp_files=()

# A temp file that is removed on exit.
tmpfile() { local f; f="$(mktemp)"; tmp_files+=("$f"); echo "$f"; }

# The ready slot columns of the indexed traits, as a JSON object key -> column.
ready_slots() {
  pg -At -v ws="$ws" -f - <<'SQL'
SELECT coalesce(json_object_agg(key, slot), '{}') FROM trait_definitions
WHERE workspace_id = :'ws' AND slot_state = 'ready';
SQL
}

# explain_to VARIANT OUTFILE [HI]: the EXPLAIN (ANALYZE, BUFFERS) of the mirrored statement; the
# slot columns of the ready traits are passed along.
explain_to() {
  local variant="$1" out="$2" hi="${3:-}" slots args=()
  slots="$(ready_slots)"
  args=(-v "ws=$ws" -v "variant=$variant" -v mode=explain)
  [ -z "$hi" ] || args+=(-v "hi=$hi")
  for pair in plan:slot_plan age:slot_age marketing_opt_in:slot_opt; do
    local slot
    slot="$(jq -r --arg k "${pair%%:*}" '.[$k] // empty' <<<"$slots")"
    if [ -n "$slot" ]; then args+=(-v "${pair##*:}=$slot"); fi
  done
  pg -At "${args[@]}" -f - <"$m4_dir/explain.sql" >"$out"
}

# mirror_count VARIANT HI: the count of the mirrored statement.
mirror_count() {
  local variant="$1" hi="$2" slots args=()
  slots="$(ready_slots)"
  args=(-v "ws=$ws" -v "variant=$variant" -v mode=count -v "hi=$hi")
  for pair in plan:slot_plan age:slot_age marketing_opt_in:slot_opt; do
    local slot
    slot="$(jq -r --arg k "${pair%%:*}" '.[$k] // empty' <<<"$slots")"
    if [ -n "$slot" ]; then args+=(-v "${pair##*:}=$slot"); fi
  done
  pg -At "${args[@]}" -f - <"$m4_dir/explain.sql"
}

# plan_indexes FILE: the distinct index names an EXPLAIN plan scans, as a JSON array.
plan_indexes() {
  { grep -oE 'Index (Only )?Scan( Backward)? using [a-z0-9_]+|Bitmap Index Scan on [a-z0-9_]+' "$1" || true; } \
    | sed -E 's/.* (using|on) //' | sort -u | jq -R . | jq -sc .
}

preview_body() { # DEFINITION_FILE -> a request file
  local f
  f="$(tmpfile)"
  jq -c --slurpfile d "$1" -n '{definition: $d[0], sample_size: 20}' >"$f"
  echo "$f"
}

# preview_series PHASE DEFINITION_NAME VARIANT TARGET_S: the warm-up, the timed runs, the plan.
preview_series() {
  local phase="$1" def="$2" variant="$3" target="$4"
  local body out samples i res status secs
  body="$(preview_body "$m4_dir/defs/$def.json")"
  out="$(tmpfile)"
  samples="$(tmpfile)"
  log "$phase: warm-up and $runs timed previews of $def"
  res="$(timed_post "/workspaces/$ws/segments/preview" "$body" "$out")"
  jq -nc --arg r "$res" '($r | split(" ")) as $x | {status: ($x[0] | tonumber), seconds: ($x[1] | tonumber)}' \
    >"$M4_OUT/$phase.warmup.json"
  for i in $(seq 1 "$runs"); do
    res="$(timed_post "/workspaces/$ws/segments/preview" "$body" "$out")"
    status="${res% *}"
    secs="${res#* }"
    jq -c --arg s "$status" --arg t "$secs" \
      '{status: ($s | tonumber), seconds: ($t | tonumber), count: .count, compiled_at: .compiled_at, code: .code}' \
      "$out" >>"$samples"
  done
  local last_hi
  last_hi="$(jq -r 'select(.compiled_at != null) | .compiled_at' "$samples" | tail -n 1)"
  [ -n "$last_hi" ] || last_hi="$(date -u +%FT%T.%6NZ)"
  explain_to "$variant" "$M4_OUT/$phase.explain.txt" "$last_hi"
  local mirror
  mirror="$(mirror_count "$variant" "$last_hi")"
  jq -n --arg phase "$phase" --arg def "$def" --slurpfile definition "$m4_dir/defs/$def.json" \
    --slurpfile samples "$samples" --slurpfile warm "$M4_OUT/$phase.warmup.json" \
    --argjson target "$target" --argjson indexes "$(plan_indexes "$M4_OUT/$phase.explain.txt")" \
    --arg mirror "$mirror" --arg ws "$ws" --arg at "$(date -u +%FT%TZ)" \
    --argjson stats "$(jq -s '[.[].seconds]' "$samples" | jq -c '.[]' | stats)" '
    {phase: $phase, workspace: $ws, measured_at: $at, definition_name: $def, definition: $definition[0],
     runs: ($samples | length), warmup: $warm[0], samples: $samples, seconds: $stats,
     non_200: ($samples | map(select(.status != 200)) | length),
     count: ($samples | map(.count) | map(select(. != null)) | unique),
     mirror_count_at_last_compiled_at: ($mirror | tonumber), target_p95_s: $target,
     explain_indexes: $indexes}' >"$M4_OUT/$phase.json"
  rm -f "$M4_OUT/$phase.warmup.json"
  jq -c '{phase, runs, seconds, non_200, count, mirror_count_at_last_compiled_at, explain_indexes}' "$M4_OUT/$phase.json"
}

phase_verify() {
  local body out res hi got status api_count
  out="$(tmpfile)"
  local counts='{}'
  for def in trait_only agg90 sum_category90; do
    body="$(preview_body "$m4_dir/defs/$def.json")"
    res="$(timed_post "/workspaces/$ws/segments/preview" "$body" "$out")"
    status="${res% *}"
    if [ "$status" = 200 ]; then
      hi="$(jq -r .compiled_at "$out")"
      api_count="$(jq .count "$out")"
    else
      # A preview past the API's 10 s limit is a 503: there is no count to compare, only the status.
      hi="$(date -u +%FT%T.%6NZ)"
      api_count=null
    fi
    got="$(pg -At -v "ws=$ws" -v "hi=$hi" -f - <"$m4_dir/verify.sql")"
    counts="$(jq -c --arg d "$def" --arg hi "$hi" --argjson st "$status" --argjson api "$api_count" --argjson sql "$got" \
      '. + {($d): {api_status: $st, api_preview: $api, handwritten_sql: $sql[$d],
                   equal: (if $api == null then null else $api == $sql[$d] end), compiled_at: $hi}}' <<<"$counts")"
  done
  jq . <<<"$counts" >"$M4_OUT/verify.json"
  jq -c . "$M4_OUT/verify.json"
  jq -e 'all(.[]; .equal != false)' "$M4_OUT/verify.json" >/dev/null || die "a preview count differs from the hand-written count"
}

phase_index() {
  local keys=(plan age marketing_opt_in) key t0 t1 state entries='[]' deadline
  for key in "${keys[@]}"; do
    log "index: $key"
    t0="$(date +%s.%N)"
    api POST "/workspaces/$ws/traits/$key/index" >/dev/null
    deadline=$(( $(date +%s) + ${M4_INDEX_TIMEOUT:-7200} ))
    while :; do
      state="$(api GET "/workspaces/$ws/traits" | jq -r --arg k "$key" '.[] | select(.key == $k) | .index_state // "none"')"
      [ "$state" != ready ] || break
      [ "$(date +%s)" -lt "$deadline" ] || die "the backfill of $key did not finish"
      sleep 0.5
    done
    t1="$(date +%s.%N)"
    entries="$(jq -c --arg k "$key" --argjson s "$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", b - a }')" \
      '. + [{trait: $k, seconds_to_ready: $s}]' <<<"$entries")"
  done
  jq -n --argjson entries "$entries" --argjson slots "$(ready_slots)" --arg ws "$ws" --arg at "$(date -u +%FT%TZ)" \
    --argjson people "$(pg -At -v ws="$ws" -f - <<<"SELECT count(*) FROM people WHERE workspace_id = :'ws';")" \
    '{phase: "index", workspace: $ws, measured_at: $at, people: $people, backfills: $entries, slots: $slots,
      note: "client clock from the POST to the first poll that reports ready (0.5 s poll)"}' >"$M4_OUT/index.json"
  jq -c . "$M4_OUT/index.json"
}

# job_wait SEGMENT T_CALL: waits for the segment's refresh job (kind segment.refresh) to leave the
# queue and prints "queue_s run_s total_s" on the database clock: created_at to the first poll that
# sees it running, that poll to the first poll that finds no job, and the whole. A job found dead
# is a failure.
job_wait() {
  local seg="$1" t_call="$2" t0 t_run="" t1 state
  t0="$(pg -At -v seg="$seg" -f - <<'SQL'
SELECT created_at FROM jobs WHERE kind = 'segment.refresh' AND payload ->> 'segment_id' = :'seg'
ORDER BY created_at DESC LIMIT 1;
SQL
)"
  [ -n "$t0" ] || t0="$t_call"
  while :; do
    state="$(pg -At -v seg="$seg" -f - <<'SQL'
SELECT state FROM jobs WHERE kind = 'segment.refresh' AND payload ->> 'segment_id' = :'seg'
ORDER BY created_at DESC LIMIT 1;
SQL
)"
    if [ -z "$state" ]; then break; fi
    [ "$state" != dead ] || die "the refresh job of $seg is dead"
    if [ "$state" = running ] && [ -z "$t_run" ]; then t_run="$(pg -At -c 'SELECT clock_timestamp()')"; fi
    sleep 0.3
  done
  t1="$(pg -At -c 'SELECT clock_timestamp()')"
  [ -n "$t_run" ] || t_run="$t1"
  pg -At -v t0="$t0" -v tr="$t_run" -v t1="$t1" -f - <<'SQL'
SELECT round(extract(epoch FROM :'tr'::timestamptz - :'t0'::timestamptz)::numeric, 3) || ' ' ||
       round(extract(epoch FROM :'t1'::timestamptz - :'tr'::timestamptz)::numeric, 3) || ' ' ||
       round(extract(epoch FROM :'t1'::timestamptz - :'t0'::timestamptz)::numeric, 3);
SQL
}

phase_refresh() { # PHASE DEFINITION_NAME TARGET_S
  local phase="$1" def="$2" target="$3" i seg name t_call timing created manual entries='[]' detail
  for i in $(seq 1 "$refresh_runs"); do
    name="m4-$def-run$i-$(date -u +%H%M%S)"
    log "$phase: run $i of $refresh_runs"
    t_call="$(pg -At -c 'SELECT clock_timestamp()')"
    created="$(api_json POST "/workspaces/$ws/segments" \
      "$(jq -nc --arg n "$name" --slurpfile d "$m4_dir/defs/$def.json" '{name: $n, definition: $d[0], cadence_minutes: 10080}')")"
    seg="$(jq -r .id <<<"$created")"
    timing="$(job_wait "$seg" "$t_call")"
    detail="$(api GET "/workspaces/$ws/segments/$seg" | jq -c '{member_count, computed_version}')"
    t_call="$(pg -At -c 'SELECT clock_timestamp()')"
    [ "$(api POST "/workspaces/$ws/segments/$seg/refresh" | jq -r .queued)" = true ] || die "the manual refresh was not queued"
    manual="$(job_wait "$seg" "$t_call")"
    entries="$(jq -c --arg run "$i" --arg seg "$seg" --arg a "$timing" --arg b "$manual" --argjson d "$detail" '
      ($a | split(" ") | map(tonumber)) as $x | ($b | split(" ") | map(tonumber)) as $y
      | . + [{run: ($run | tonumber), segment: $seg, members: $d,
              first_refresh: {queue_s: $x[0], run_s: $x[1], total_s: $x[2]},
              manual_refresh: {queue_s: $y[0], run_s: $y[1], total_s: $y[2]}}]' <<<"$entries")"
    # An archived segment is not evaluated again, so it cannot skew a later phase.
    api POST "/workspaces/$ws/segments/$seg/archive" >/dev/null
  done
  jq -n --arg phase "$phase" --arg def "$def" --slurpfile definition "$m4_dir/defs/$def.json" \
    --argjson runs "$entries" --argjson target "$target" --arg ws "$ws" --arg at "$(date -u +%FT%TZ)" '
    def med: sort | if length == 0 then null elif length % 2 == 1 then .[(length - 1) / 2] else (.[length / 2 - 1] + .[length / 2]) / 2 end;
    {phase: $phase, workspace: $ws, measured_at: $at, definition_name: $def, definition: $definition[0], runs: $runs,
     median_first_refresh_total_s: ($runs | map(.first_refresh.total_s) | med),
     median_manual_refresh_total_s: ($runs | map(.manual_refresh.total_s) | med),
     target_s: $target,
     note: "queue_s and run_s are only as fine as the 0.3 s poll (run_s is 0 when no poll caught the job running); total_s is the figure. first_refresh is the save-triggered initial refresh of a new segment (every member enters); manual_refresh is POST .../refresh on it, a full recompute that diffs against the open rows"}' \
    >"$M4_OUT/$phase.json"
  jq -c '{phase, median_first_refresh_total_s, median_manual_refresh_total_s, target_s}' "$M4_OUT/$phase.json"
}

# pending_marks: dirty marks of the workspace plus ready or running re-evaluation jobs.
pending_marks() {
  pg -At -v ws="$ws" -f - <<'SQL'
SELECT (SELECT count(*) FROM person_changes WHERE workspace_id = :'ws')
     + (SELECT count(*) FROM jobs WHERE workspace_id = :'ws' AND kind = 'segment.reevaluate')
     + (SELECT count(*) FROM ingest_inbox);
SQL
}

phase_incremental() {
  [ -x "$loadgen" ] || die "no loadgen at $loadgen; cargo build --release -p zirv-flow-loadgen"
  [ -s "$M4_STATE/loadgen.key" ] || die "no ingest key; run setup.sh"
  local name created seg since t_call timing before after deadline pending latency
  name="m4-incremental-$(date -u +%H%M%S)"
  t_call="$(pg -At -c 'SELECT clock_timestamp()')"
  created="$(api_json POST "/workspaces/$ws/segments" \
    "$(jq -nc --arg n "$name" --slurpfile d "$m4_dir/defs/agg90.json" '{name: $n, definition: $d[0], cadence_minutes: 10080}')")"
  seg="$(jq -r .id <<<"$created")"
  timing="$(job_wait "$seg" "$t_call")"
  before="$(api GET "/workspaces/$ws/segments/$seg" | jq -c '{member_count, computed_version}')"
  log "incremental: segment $seg refreshed ($before); loadgen $inc_rate/s for $inc_seconds s over $inc_pool people"
  since="$(pg -At -c "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")"
  "$loadgen" --url "$api_base" --key-file "$M4_STATE/loadgen.key" --mode track --rate "$inc_rate" \
    --duration "$inc_seconds" --people "$inc_pool" --run-id "m4-$(date -u +%H%M%S)" \
    --out "$M4_OUT/incremental-loadgen.json" >&2
  log "incremental: loadgen done; waiting for the inbox and the marks to drain"
  deadline=$(( $(date +%s) + drain_timeout ))
  while :; do
    pending="$(pending_marks)"
    [ "$pending" = 0 ] && break
    [ "$(date +%s)" -lt "$deadline" ] || { log "marks still pending: $pending"; break; }
    sleep 2
  done
  latency="$(pg -At -v ws="$ws" -v segment="$seg" -v since="$since" -f - <"$m4_dir/incremental-latency.sql")"
  after="$(api GET "/workspaces/$ws/segments/$seg" | jq -c '{member_count, computed_version}')"
  jq -n --arg seg "$seg" --arg since "$since" --argjson before "$before" --argjson after "$after" \
    --argjson latency "$latency" --arg pending "$pending" --arg ws "$ws" --arg at "$(date -u +%FT%TZ)" \
    --argjson rate "$inc_rate" --argjson seconds "$inc_seconds" --argjson pool "$inc_pool" \
    --slurpfile lg "$M4_OUT/incremental-loadgen.json" '
    {phase: "incremental", workspace: $ws, measured_at: $at, segment: $seg, since: $since,
     definition_name: "agg90", loadgen: {rate_per_s: $rate, seconds: $seconds, people: $pool,
       total: ($lg[0].total // null)},
     members_before: $before, members_after: $after, pending_after_wait: ($pending | tonumber),
     latency_s: $latency, target_p95_s: 60,
     note: "recorded_at of the segment.entered decision minus the causing event received_at; recorded_at is the start of the re-evaluating transaction (slight under-estimate)"}' \
    >"$M4_OUT/incremental.json"
  jq -c '{phase, members_before, members_after, pending_after_wait, latency_s}' "$M4_OUT/incremental.json"
  # Leave the database as the other phases expect it: nothing re-evaluates this segment any more.
  api POST "/workspaces/$ws/segments/$seg/archive" >/dev/null
}

[ "$#" -gt 0 ] || { sed -n '2,/^# Usage/p' "$0" >&2; exit 2; }
for phase in "$@"; do
  case "$phase" in
    verify) phase_verify ;;
    preview-jsonb) preview_series preview-jsonb trait_only trait_jsonb 3 ;;
    index) phase_index ;;
    preview-slots) preview_series preview-slots trait_only trait_slots 3 ;;
    preview-agg) preview_series preview-agg agg90 agg90 15 ;;
    preview-sum) preview_series preview-sum sum_category90 sum90 15 ;;
    refresh-trait) phase_refresh refresh-trait trait_only 120 ;;
    refresh-agg) phase_refresh refresh-agg agg90 600 ;;
    incremental) phase_incremental ;;
    all)
      for p in verify preview-jsonb index preview-slots preview-agg preview-sum refresh-trait refresh-agg incremental; do
        "$0" "$p"
      done ;;
    *) die "unknown phase $phase" ;;
  esac
done
