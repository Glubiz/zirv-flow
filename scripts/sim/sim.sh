#!/usr/bin/env bash
# The simulated production environment (design #513). See `sim.sh help`.
set -euo pipefail
umask 022
# shellcheck source=scripts/sim/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

usage() {
  cat <<'TEXT'
usage: sim.sh <command>
  init                      create .state/ with the CA, leaf certificate, secrets and control file
  up [--profile full|small|x5] init, build the tools image, start and wait for every service
  down                      stop the stack, keep the volumes
  reset                     remove the stack, its volumes and .state/ (this project only)
  logs <service> [args]     docker compose logs
  credentials               print the generated users, passwords and keys
  status                    container health, fixture counters and traffic
  seed [sources|ingest-people|all] [--people N]  load the company sources and/or the people backbone
  traffic [on|off]          start or stop the four generators
  rate N | burst [factor] [seconds] | pause | resume | export-now   traffic controls
  provision [stage...]      provision the workspace through the API (stages: base registry people connections
                            sending segments templates syncs gate flows; default all)
  start [--profile full|small|x5]  up, provision, ready gate, traffic on, then the event-driven flows
  verify [--provision] [group...]  the checks of design 8.2, exit non-zero when one fails
  sync-all                  queue a run for every pull stream
  blast                     start the re-engagement flow now on the lapsed customers
TEXT
}

init_state() {
  need_tools
  mkdir -p "$STATE"/{tls,keys,dns,seed,exports}
  [[ -f "$STATE/profile" ]] || echo small > "$STATE/profile"
  if [[ ! -f "$STATE/tls/leaf.pem" ]]; then
    say 'creating the CA and leaf certificate'
    sim_tls "$STATE/tls"
  fi
  if [[ ! -f "$STATE/env" ]]; then
    umask 077
    {
      echo "SIM_PG_PASSWORD=$(random_secret)"
      echo "SIM_COMPANY_PG_PASSWORD=$(random_secret)"
      echo "SIM_MYSQL_ROOT_PASSWORD=$(random_secret)"
      echo "SIM_FLOW_READER_PASSWORD=$(random_secret)"
      echo "SIM_OWNER_PASSWORD=$(random_secret)"
      echo "SIM_SMTP_PASSWORD=sim"
    } > "$STATE/env"
    umask 022
  fi
  [[ -f "$STATE/dns/records.json" ]] || echo '{}' > "$STATE/dns/records.json"
  [[ -f "$STATE/control.json" ]] ||
    echo '{"rate": 250, "day_seconds": 86400, "burst_factor": 1.0, "burst_until": null, "paused": false}' \
      > "$STATE/control.json"
  docker version > /dev/null 2>&1 || die 'the docker daemon is not running'
}

cmd_up() {
  local profile=
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --profile) [[ $# -ge 2 ]] || die 'missing profile'; profile=$2; shift 2 ;;
      *) die "unknown argument: $1" ;;
    esac
  done
  mkdir -p "$STATE"
  if [[ -n "$profile" ]]; then
    [[ -f "$SIM_DIR/profiles/$profile.env" ]] || die "no such profile: $profile"
    echo "$profile" > "$STATE/profile"
  fi
  init_state
  say "profile $(cat "$STATE/profile"), image tag $(env_value SIM_IMAGE_TAG)"
  compose --profile tools build sim-tools
  compose up -d --wait --wait-timeout 420
  # The disk guard and status read the database size through this role (design 1.4).
  compose exec -T zf-db psql -XqAt -v ON_ERROR_STOP=1 -U zirv -d zirv_flow <<'SQL' > /dev/null
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sim_monitor') THEN
    CREATE ROLE sim_monitor LOGIN PASSWORD 'sim_monitor';
  END IF;
END $$;
GRANT pg_monitor TO sim_monitor;
SQL
  # Seeding drops and reloads the company tables and the Kafka backlog: once per set of volumes.
  if [[ -f "$STATE/seeded-sources" ]]; then
    say 'the company sources are already seeded (sim.sh reset to start over)'
  else
    say 'seeding the company sources'
    cmd_seed sources
  fi
  say 'stack is up: http://localhost:8080 (app), :9090 (prometheus), :8025 (mailbox viewer)'
}

cmd_down() {
  [[ -f "$STATE/profile" ]] || { say 'nothing to stop'; return 0; }
  compose --profile '*' down --remove-orphans
}

cmd_reset() {
  mkdir -p "$STATE"
  [[ -f "$STATE/profile" ]] || say 'no state; removing project resources only'
  [[ -f "$STATE/profile" ]] || echo small > "$STATE/profile"
  [[ -f "$STATE/env" ]] || : > "$STATE/env"
  # Placeholder values satisfy the compose file's required variables while removing.
  SIM_PG_PASSWORD=x SIM_COMPANY_PG_PASSWORD=x SIM_MYSQL_ROOT_PASSWORD=x SIM_FLOW_READER_PASSWORD=x \
    compose --profile '*' down -v --remove-orphans
  rm -r "${STATE:?}"
  say 'removed the stack, its volumes and .state/'
}

cmd_credentials() {
  [[ -f "$STATE/env" ]] || die 'run sim.sh init first'
  echo 'owner@sim.example.com (owner, created by provisioning)'
  cat "$STATE/env"
  local file
  for file in "$STATE"/keys/*; do
    [[ -f "$file" ]] || continue
    echo "-- $(basename "$file")"
    cat "$file"
  done
}

cmd_status() {
  [[ -f "$STATE/profile" ]] || die 'not initialised; run sim.sh up'
  compose ps -a --format 'table {{.Service}}\t{{.State}}\t{{.Health}}\t{{.Status}}'
  if curl -fsS --max-time 5 http://localhost:8080/readyz > /dev/null 2>&1; then
    echo 'app: ready'
  else
    echo 'app: not ready'
  fi
  echo '-- sink arrivals'
  metric smtp-sink 8025 sim_smtp_messages_total || true
  metric mailer 8000 sim_mailer_messages_total || true
  metric receiver 8000 sim_receiver_accepted_total || true
  status_traffic
  echo '-- database'
  compose exec -T zf-db psql -XqAt -U zirv -d zirv_flow \
    -c "SELECT 'size ' || pg_size_pretty(pg_database_size('zirv_flow'))" 2> /dev/null || true
}

control_set() {
  # Atomic: write a temp file in the same directory, then rename over control.json.
  local tmp="$STATE/control.json.tmp"
  jq "$@" "$STATE/control.json" > "$tmp" && mv "$tmp" "$STATE/control.json"
}

cmd_rate() {
  [[ ${1:-} =~ ^[0-9]+$ ]] || die 'usage: sim.sh rate <events per second, daily mean>'
  # shellcheck disable=SC2016  # $r is a jq variable
  control_set --argjson r "$1" '.rate = $r'
  say "daily mean rate set to $1 events/s"
}

cmd_burst() {
  local factor=${1:-3} seconds=${2:-60}
  [[ $factor =~ ^[0-9]+([.][0-9]+)?$ && $seconds =~ ^[0-9]+$ ]] || die 'usage: sim.sh burst [factor] [seconds]'
  # shellcheck disable=SC2016  # jq variables
  control_set --argjson f "$factor" --argjson until "$(($(date +%s) + seconds))" \
    '.burst_factor = $f | .burst_until = $until'
  say "burst x$factor for ${seconds}s"
}

cmd_pause() { control_set '.paused = true | .paused_reason = "operator"'; say 'generators paused'; }
cmd_resume() { control_set '.paused = false | del(.paused_reason)'; say 'generators resumed'; }

cmd_export_now() {
  : > "$STATE/export-now"
  say 'asked gen-changes to regenerate warehouse.csv (it picks the flag up within seconds)'
}

# seed [sources|ingest-people|all] [seed.py arguments]
cmd_seed() {
  local what=${1:-all}
  [[ $# -eq 0 ]] || shift
  compose --profile data run --rm seed python -B /gen/seed.py "$what" "$@"
  [[ $what == ingest-people ]] || : > "$STATE/seeded-sources"
}

cmd_traffic() {
  case "${1:-on}" in
    on) compose --profile traffic up -d --no-deps gen-ingest gen-changes gen-kafka gen-forms ;;
    off) compose --profile traffic stop gen-ingest gen-changes gen-kafka gen-forms ;;
    *) die 'usage: sim.sh traffic on|off' ;;
  esac
}

status_traffic() {
  echo '-- traffic'
  if [[ -f "$STATE/control.json" ]]; then
    jq -r '"control: rate \(.rate) burst x\(.burst_factor) paused \(.paused)\(if .paused_reason then " (\(.paused_reason))" else "" end)"' "$STATE/control.json"
  fi
  # A generator split over several processes writes one file per part; add the parts up.
  if compgen -G "$STATE/stats/*.json" > /dev/null; then
    jq -rs 'group_by(.name)[] | {name: .[0].name, a: (map(.achieved_per_s) | add), t: (map(.target_per_s) | add), s: (map(.sent_total) | add), l: (map(.late_total) | add), d: (map(.dropped_total) | add), e: (map(.errors_total) | add)} | "\(.name): achieved \(.a * 10 | round / 10)/s of target \(.t * 10 | round / 10)/s, sent \(.s), late \(.l), dropped \(.d), errors \(.e)"' "$STATE"/stats/*.json
  fi
  zf_sql "SELECT 'people ' || count(*) FROM people WHERE merged_into IS NULL" || true
  zf_sql "SELECT 'inbox age ' || round(coalesce(extract(epoch FROM now() - min(received_at)), 0)::numeric, 1) || ' s, ' || count(*) || ' queued' FROM ingest_inbox" || true
  zf_sql "SELECT 'events (estimate) ' || coalesce(sum(n_live_tup), 0) FROM pg_stat_user_tables WHERE relname ~ '^events_(default|y[0-9]{4}m[0-9]{2})\$'" || true
  zf_sql "WITH e AS (SELECT coalesce(sum(n_live_tup), 0) AS n, coalesce(sum(pg_total_relation_size(relid)), 0) AS b FROM pg_stat_user_tables WHERE relname ~ '^events_(default|y[0-9]{4}m[0-9]{2})\$') SELECT 'db size ' || pg_size_pretty(pg_database_size('zirv_flow')) || ', events ' || pg_size_pretty(b) || ', bytes per event ' || CASE WHEN n > 0 THEN (b / n)::bigint::text ELSE 'n/a' END FROM e" || true
}

elapsed() { # elapsed <event> : seconds from the start to the event, or "not yet"
  local from at
  from=$(sed -n 's/^start //p' "$STATE/timeline" 2> /dev/null | head -1)
  at=$(sed -n "s/^$1 //p" "$STATE/timeline" 2> /dev/null | head -1)
  if [[ -n "$from" && -n "$at" ]]; then echo "$((at - from)) s"; else echo 'not yet'; fi
}

print_timeline() {
  echo "fresh start: ready gate after $(elapsed gate), traffic on after $(elapsed traffic)," \
    "backfill complete after $(elapsed backfill), event flows published after $(elapsed flows)"
}

cmd_start() {
  mkdir -p "$STATE"
  echo "start $(date +%s)" > "$STATE/timeline"
  cmd_up "$@"
  say "start: provisioning"
  bash "$SIM_DIR/provision.sh" base registry people connections sending templates syncs gate
  if [[ -n "${SIM_TRAFFIC_AFTER_BACKFILL:-}" ]]; then
    # Under the CPU cap the inbox cannot drain while the first syncs run (B findings); start traffic last.
    bash "$SIM_DIR/provision.sh" flows
    stamp traffic
    cmd_traffic on
  else
    stamp traffic
    cmd_traffic on
    bash "$SIM_DIR/provision.sh" flows
  fi
  print_timeline
}

main() {
  local command=${1:-help}
  [[ $# -eq 0 ]] || shift
  case "$command" in
    help | -h | --help) usage ;;
    init) init_state; say "state is in $STATE" ;;
    up) cmd_up "$@" ;;
    down) cmd_down ;;
    reset) cmd_reset ;;
    logs) compose logs --no-color "$@" ;;
    credentials) cmd_credentials ;;
    status) cmd_status ;;
    rate) cmd_rate "$@" ;;
    burst) cmd_burst "$@" ;;
    pause) cmd_pause ;;
    resume) cmd_resume ;;
    export-now) cmd_export_now ;;
    seed) cmd_seed "$@" ;;
    traffic) cmd_traffic "$@" ;;
    provision) bash "$SIM_DIR/provision.sh" "$@" ;;
    start) cmd_start "$@" ;;
    verify) bash "$SIM_DIR/verify.sh" "$@" ;;
    sync-all) bash "$SIM_DIR/provision.sh" sync-all ;;
    blast) bash "$SIM_DIR/provision.sh" blast ;;
    *) usage >&2; exit 2 ;;
  esac
}

main "$@"
