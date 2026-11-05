#!/usr/bin/env bash
# The simulated production environment (design #513). See `sim.sh help`.
set -euo pipefail
umask 022
# Git Bash would rewrite /CN=... and container paths in arguments otherwise.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

SIM_PROJECT=zirv-flow-sim
SIM_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SIM_DIR=$(
  if command -v cygpath > /dev/null 2>&1; then cygpath -m "$SIM_DIR"; else printf '%s' "$SIM_DIR"; fi
)
STATE="$SIM_DIR/.state"
# shellcheck source=scripts/sim/lib.sh
source "$SIM_DIR/lib.sh"

usage() {
  cat <<'TEXT'
usage: sim.sh <command>
  init                      create .state/ with the CA, leaf certificate, secrets and control file
  up [--profile full|small] init, build the tools image, start and wait for every service
  down                      stop the stack, keep the volumes
  reset                     remove the stack, its volumes and .state/ (this project only)
  logs <service> [args]     docker compose logs
  credentials               print the generated users, passwords and keys
  status                    container health and fixture counters
Later groups add: provision, start, verify, rate, burst, pause, resume, sync-all, export-now, blast.
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
  if [[ -f "$SIM_DIR/gen/seed.py" ]]; then
    say 'seeding the company sources'
    compose --profile data run --rm seed
  fi
  say 'stack is up: http://localhost:8080 (app), :9090 (prometheus), :8025 (mailbox viewer)'
}

cmd_down() {
  [[ -f "$STATE/profile" ]] || { say 'nothing to stop'; return 0; }
  compose --profile '*' down --remove-orphans
}

cmd_reset() {
  [[ -f "$STATE/profile" ]] || say 'no state; removing project resources only'
  [[ -f "$STATE/profile" ]] || echo small > "$STATE/profile"
  [[ -f "$STATE/env" ]] || { mkdir -p "$STATE"; : > "$STATE/env"; }
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
  echo '-- database'
  compose exec -T zf-db psql -XqAt -U zirv -d zirv_flow \
    -c "SELECT 'size ' || pg_size_pretty(pg_database_size('zirv_flow'))" 2> /dev/null || true
}

later() { die "$1 is added by a later group (A2: traffic and seed, B: provisioning and verify)"; }

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
    provision | start | verify | rate | burst | pause | resume | sync-all | export-now | blast)
      later "$command" ;;
    *) usage >&2; exit 2 ;;
  esac
}

main "$@"
