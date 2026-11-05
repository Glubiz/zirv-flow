#!/usr/bin/env bash
# M8 reference-resource emulation. Uses prebuilt tools/images only; see README section 13.
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

usage() {
  cat <<'HELP'
Usage: bash scripts/bench/m8/run.sh <command>
  up                   Start the isolated bench stack (pre-pulled versioned image)
  seed <name>          SQL seed: people | events | enrollments (resumable)
  smoke                Separate small stack; 60s ingest, 5s debugger, 3,000 jobs
  ingest | burst | batch | debugger | explorer | jobs | timers | decisions | send | render | segments
  ingest               Track load at 2,000/s plus inbox-to-applied latency (ingest-applied.json)
  burst                10,000/s batch burst (M8_BURST_SECONDS, 600) at a raised key limit, then the inbox drain time
  all                  Run all registered benchmarks; note unregistered ones
  debugger             Watch streams before track load (M8_DEBUGGER_CLIENTS=1; full: 20)
  explorer             90-day queries at default/15000 ms, alongside track ingest
  jobs                 No-op throughput/fairness, uncapped then M8_JOBS_CAP=4
  timers               Re-arm waiting seed; measure without and with track load
  pg-settings <a|b>    Recreate only PostgreSQL with settings side A or B and verify them (SHOW)
  --self-test          Shell tests; no Docker or database
  --help               Show this help
Env: M8_PG_SETTINGS=a|b runs an arm on that settings side; M8_IO_DIAG=1 adds the I/O diagnostics.
A/B driver: bash scripts/bench/m8/ab-settings.sh <ingest|timers> <runs-per-side>.
See scripts/bench/README.md section 13 for scale, disk and prebuilt-tool requirements.
HELP
}

# One case arm per measurement. New issues replace their arm with cmd=(...) and return 0.
# The command writes raw JSON beneath $raw; run_bench supplies metadata with write_data.
bench_command() {
  case "$1" in
    ingest) cmd=(bash "$m8_dir/ingest.sh" "$raw" "$pool") ;;
    burst) cmd=(bash "$m8_dir/burst.sh" "$raw" "$pool") ;;
    batch) cmd=(env M8_BATCH_SIZE="${M8_BATCH_SIZE:-500}" bash "$m8_dir/ingest.sh" "$raw" "$pool") ;;
    segments) cmd=(bash "$root/scripts/bench/m4/measure.sh" ${M8_SEGMENTS_PHASES:-all}) ;;
    render) cmd=(bash "$root/scripts/bench/m7/render.sh" "$raw") ;;
    send) cmd=(bash "$root/scripts/bench/m6/run.sh" "m8-${raw##*/}") ;;
    debugger) cmd=(bash "$m8_dir/debugger.sh" "$raw" "$pool") ;;
    explorer) cmd=(bash "$m8_dir/explorer.sh" "$raw" "$pool") ;;
    jobs) cmd=(bash "$m8_dir/jobs.sh" "$raw") ;;
    timers) cmd=(bash "$m8_dir/timers.sh" "$raw" "$pool") ;;
    decisions) cmd=(bash "$m8_dir/decisions.sh" "$raw") ;;
  esac
}

up() {
  validate_cpus
  compose up -d --no-build --wait --wait-timeout 300 postgres zirv-flow-key zirv-flow
  compose up -d --no-build --wait --wait-timeout 300 zirv-flow-worker zirv-flow-scheduler
  pg -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
  (
    # Reuse M4's authentication helpers; seeds themselves never call the product API.
    # shellcheck source=../m4/common.sh
    . "$m8_dir/../m4/common.sh"
    if [ ! -f "$M4_STATE/owner.pass" ]; then
      (umask 077; python3 -I -c 'import secrets; print(secrets.token_urlsafe(24))' >"$M4_STATE/owner.pass")
    fi
    if ! login; then
      compose exec -T zirv-flow /usr/local/bin/zirv-flow admin create-owner --email "$M4_EMAIL" <"$M4_STATE/owner.pass" >/dev/null
      login || die 'cannot sign in as bench owner'
    fi
  )
}

seed_manifest() {
  mkdir -p "$M4_STATE/.."
  pg -At -f - >"$M4_STATE/../seeds.json.tmp" <<'SQL'
SELECT json_build_object('targets', sizes, 'seed_at', seed_at, 'wake_at', wake_at,
  'event_window_days', 365, 'source', 'm8bench.progress (committed seed rows, not current live counts)',
  'workspaces', (SELECT json_agg(json_build_object('workspace_id', m8bench.ws(g),
    'people', m8bench.size((sizes->>'people')::bigint,g),
    'events', m8bench.size((sizes->>'events')::bigint,g),
    'enrollments', m8bench.size((sizes->>'enrollments')::bigint,g))) FROM generate_series(1,3) g),
  'committed', (SELECT coalesce(json_object_agg(kind,n), '{}'::json) FROM
    (SELECT kind,sum(last_n) n FROM m8bench.progress GROUP BY kind) p)) FROM m8bench.config;
SQL
  python3 -I -c 'import json,sys; assert isinstance(json.load(open(sys.argv[1])), dict)' "$M4_STATE/../seeds.json.tmp"
  mv "$M4_STATE/../seeds.json.tmp" "$M4_STATE/../seeds.json"
}

seed() {
  local kind="$1" sample='{}' slot count lo hi done table
  local tables=()
  seed_preflight "$kind"
  if [ "$kind" != people ]; then
    done="$(pg -At -c "SELECT coalesce(sum(last_n),0) FROM m8bench.progress WHERE kind='people'")"
    [ "$done" = "$M8_PEOPLE" ] || die 'seed people must finish first'
  fi
  if [ "$kind" = enrollments ]; then
    sample="$(python3 -I -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))))' \
      "$root/docs/benchmarks/data/m5/sample/spread.json")"
  fi
  pg -v kind="$kind" -v people="$M8_PEOPLE" -v events="$M8_EVENTS" -v enrollments="$M8_ENROLLMENTS" \
    -v chunk="$M8_CHUNK" -v sample="$sample" -f /bench/m8/seed.sql >&2
  if [ "$kind" = enrollments ]; then
    for slot in 1 2 3; do
      count=$((M8_ENROLLMENTS * 2 / 5))
      [ "$slot" != 3 ] || count=$((M8_ENROLLMENTS - 2 * count))
      done="$(pg -At -c "SELECT coalesce((SELECT last_n FROM m8bench.progress WHERE kind='enrollments' AND workspace_id=m8bench.ws($slot)),0)")"
      for ((lo=done+1; lo<=count; lo+=M8_CHUNK)); do
        hi=$((lo + M8_CHUNK - 1)); [ "$hi" -le "$count" ] || hi="$count"
        pg -v slot="$slot" -v lo="$lo" -v hi="$hi" -f /bench/m8/enrollments.sql >&2
        log "enrollments workspace $slot: $hi/$count"
      done
    done
  fi
  case "$kind" in
    people) tables=(people identities) ;;
    events) tables=(events) ;;
    enrollments) tables=(enrollments step_executions jobs flow_entries decision_records m5bench.rows) ;;
  esac
  for table in "${tables[@]}"; do
    pg -c "VACUUM (ANALYZE, PARALLEL 0) $table" >&2
  done
  (
    # shellcheck source=../m4/common.sh
    . "$m8_dir/../m4/common.sh"
    state_put M4_WS "$(pg -At -c 'SELECT m8bench.ws(1)')"
  )
  seed_manifest
}

prepare_send() {
  # PEOPLE (the audience) defaults to 20000 and may be set up to the first workspace's people (for example 60000).
  # M8_WORKER_CONCURRENCY reaches the M6 run as WORKER_CONCURRENCY (its worker processes read BENCH_WORKER_CONCURRENCY).
  positive_integer M8_WORKER_CONCURRENCY "${M8_WORKER_CONCURRENCY:-4}"
  if [ -n "${PEOPLE:-}" ]; then
    positive_integer PEOPLE "$PEOPLE"
    [ "$PEOPLE" -le "$pool" ] || die "PEOPLE ($PEOPLE) exceeds the first workspace's $pool people"
  else
    PEOPLE=$((pool < 20000 ? pool : 20000))
  fi
  export PEOPLE WORKERS=1 WORKER_CONCURRENCY="${M8_WORKER_CONCURRENCY:-4}"
  export RUN_TIMEOUT_S="${RUN_TIMEOUT_S:-$((PEOPLE / 50 + 300))}"
  export M6_OUT="$raw" M6_SINK_CONTAINER="${M4_PROJECT}-sink" M6_SINK_HOST="${M4_PROJECT}-sink"
  # The sink is outside all three allocations; the default requires a 17th logical CPU.
  export M6_SINK_CPUSET="${M6_SINK_CPUSET:-16}"
  export SENDBENCH="${SENDBENCH:-$root/target/release/sendbench}"
  [ -x "$SENDBENCH" ] || die "no prebuilt sendbench at $SENDBENCH"
  [ -f "$M8_STATE/send-tools/linux/sendbench" ] || die "install the CI Linux sendbench artifact at $M8_STATE/send-tools/linux/sendbench"
  mkdir -p "$M4_STATE/../sink"
  ln -s "$M8_STATE/send-tools/linux" "$raw/linux"
  ln -s "$M4_STATE/../sink" "$raw/sink"
  compose -f "$root/scripts/bench/m6/compose.m6.yaml" up -d --no-build --wait
  bash "$root/scripts/bench/m6/sink.sh" start
  (
    # shellcheck source=../m4/common.sh
    . "$m8_dir/../m4/common.sh"
    state_put M6_WS "$(state_get M4_WS)"
  )
  bash "$root/scripts/bench/m6/setup.sh"
  # M6's measurement is reused; its API people seeder is replaced by SQL bench fixtures.
  pg -v n="$PEOPLE" -f - <<'SQL'
BEGIN;
INSERT INTO identities (id, workspace_id, person_id, kind, value)
SELECT md5('m8/send/'||g)::uuid, m8bench.ws(1), md5(m8bench.ws(1)::text||'/p/'||g)::uuid,
       'email', 'sb-'||(g-1)||'@bench.example.com' FROM generate_series(1,:n) g
ON CONFLICT DO NOTHING;
INSERT INTO consent (workspace_id,person_id,channel,status,explicit,changed_at)
SELECT m8bench.ws(1), md5(m8bench.ws(1)::text||'/p/'||g)::uuid, 'email','subscribed',true,now()
FROM generate_series(1,:n) g ON CONFLICT DO NOTHING;
COMMIT;
SQL
}

run_bench() {
  local bench="$1" raw pool topology rc=0
  local cmd=()
  pool=$((M8_PEOPLE * 2 / 5)); [ "$pool" -le 1000000 ] || pool=1000000
  raw="$M4_STATE/../runs/$bench-$(date -u +%Y%m%dT%H%M%S)-$$"
  bench_command "$bench" || return $?
  if [ "$bench" = burst ]; then
    # A shortened burst is a smoke run: the data lands under smoke/ and the gate rejects it as a citation.
    burst_seconds >/dev/null
    ! burst_shortened || smoke=1
    trap run_exit EXIT
    raise_serve_limits
  fi
  if [ "$bench" = decisions ]; then
    # The A/B builds its own containers and stops the M8 stack for the run: no M8 topology, seeds or instrumentation.
    host_pg_setup
    mkdir -p "$raw"
    topology="$raw/topology.ndjson"
    "${cmd[@]}" || rc=$?
    write_data "$bench" "$rc" "$raw" "$topology" "${cmd[@]}"
    return "$rc"
  fi
  # Every arm but render runs the wait-event sampler (instrument_begin), which needs host psql.
  [ "$bench" = render ] || host_pg_setup
  validate_cpus
  if [ "$bench" != render ] && [ -n "${M8_PG_SETTINGS:-}" ]; then switch_pg_settings "$M8_PG_SETTINGS"; fi
  seed_manifest
  mkdir -p "$raw"
  export M4_OUT="$raw" M4_INC_SECONDS="$M8_SECONDS" M4_INC_RATE="$M8_RATE" M4_INC_POOL="$pool"
  export RENDER_BENCH_BIN="${RENDER_BENCH_BIN:-$root/target/release/renderbench}"
  export RENDER_BENCH_CONSTRAINT='host tool, CPU affinity unchanged; Docker app topology is recorded separately'
  if [ "$bench" != render ]; then bash "$root/scripts/bench/m4/setup.sh"; fi
  if [ "$bench" = send ]; then prepare_send; fi
  topology="$raw/topology.ndjson"
  capture_topology "$topology"
  if [ "$bench" != render ]; then
    assert_no_samplers
    trap run_exit EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    quiet_start
    instrument_begin "$raw"
  fi
  log "running $bench ($label)"
  "${cmd[@]}" || rc=$?
  if [ "$bench" != render ] && ! instrument_end "$raw"; then
    log "instrumentation incomplete for $bench"
    [ "$rc" != 0 ] || rc=1
  fi
  # After instrument_end: the restart resets the serve metrics that instrument_end scrapes.
  restore_serve_limits || { log 'could not restore the serve limits'; [ "$rc" != 0 ] || rc=1; }
  if [ "$rc" = 0 ]; then finish_topology "$topology"; fi
  write_data "$bench" "$rc" "$raw" "$topology" "${cmd[@]}"
  return "$rc"
}

# The final topology check follows an image switch the run made (the check itself stays): the containers must all
# run the tag the serve container runs now, and the switch is recorded in the topology and the report.
finish_topology() {
  local topology="$1" end_tag
  end_tag="$(running_image_tag)"
  capture_topology "$topology" "$end_tag"
  if [ "$end_tag" != "$BENCH_IMAGE_TAG" ]; then
    log "image changed during the run: $BENCH_IMAGE_TAG -> $end_tag"
    printf '{"image_switch":{"from":"%s","to":"%s"}}\n' "$BENCH_IMAGE_TAG" "$end_tag" >>"$topology"
    export BENCH_IMAGE_TAG="$end_tag"
  fi
}

run_all() {
  local bench raw='' pool=1 reason issue ran=() skipped=() failed=()
  local cmd=()
  # Each benchmark runs in its own shell so a failed prerequisite cannot disable errexit.
  for bench in "${benches[@]}"; do
    if reason="$(bench_command "$bench" 2>&1)"; then
      if bash "$m8_dir/run.sh" "$bench"; then ran+=("$bench"); else failed+=("$bench"); fi
    else
      issue=''
      [[ "$reason" =~ \#[0-9]+ ]] && issue="${BASH_REMATCH[0]}"
      log "skipping unregistered $bench"
      skipped+=("$bench${issue:+ ($issue)}")
    fi
  done
  log "summary: ran: ${ran[*]:-none}; failed: ${failed[*]:-none}; skipped (unregistered): ${skipped[*]:-none}"
  [ "${#failed[@]}" = 0 ] || return 1
}

main() {
  invocation=(bash scripts/bench/m8/run.sh "$@")
  smoke="${M8_SMOKE:-}"
  case "${1:---help}" in
    --help | -h | help) usage; return ;;
    --self-test) bash "$m8_dir/self-test.sh"; return ;;
    seed)
      [ "$#" = 2 ] || die 'seed requires one name: people, events or enrollments'
      case "$2" in people | events | enrollments) ;; *) die "unknown seed: $2" ;; esac ;;
    pg-settings)
      [ "$#" = 2 ] || die 'pg-settings requires a or b'
      case "$2" in a | b) ;; *) die "unknown settings side: $2" ;; esac ;;
    up | smoke | all | ingest | burst | batch | debugger | explorer | jobs | timers | decisions | send | render | segments)
      [ "$#" = 1 ] || die 'unexpected arguments' ;;
    *) die "unknown subcommand: $1" ;;
  esac
  [ "$1" != smoke ] || { smoke=1; export M8_SMOKE=1; }
  [ "$1" != pg-settings ] || export M8_PG_SETTINGS="$2"
  configure
  case "$1" in
    pg-settings) host_pg_setup; switch_pg_settings "$2"; return ;;
    ingest | burst | batch | debugger | explorer) host_pg_setup ;;
    jobs | timers | smoke | all) host_pg_setup; require_job_sampler ;;
  esac
  case "$1" in
    up) up ;;
    seed) seed "$2" ;;
    smoke) up; seed people; seed events; seed enrollments; run_all ;;
    all) run_all ;;
    *) run_bench "$1" ;;
  esac
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
