#!/usr/bin/env bash
# The manual restore drill (#398): pg_dump, an erasure, pg_restore into a fresh database, the
# erasure replay from the ledger directory, then assertions through SQL and the API. It prints a
# markdown log of counts and durations, never an identifier value.
#
# It only ever creates and drops databases whose names start with zf_drill_, and refuses any
# other name before it does anything.
#
# Environment:
#   ZF_DRILL_BIN          zirv-flow binary (default target/debug/zirv-flow, with .exe on Windows)
#   ZF_DRILL_APP_BASE     server URL prefix the binary connects to, no database name
#                         (default postgres://postgres:postgres@127.0.0.1:5432)
#   ZF_DRILL_TOOL_BASE    the same server as the PostgreSQL tools see it (default: the app base)
#   ZF_DRILL_PG_PREFIX    words put before psql, pg_dump and pg_restore, for example
#                         "docker exec -i zf-pg16d" when the tools are inside a container
#   ZF_DRILL_PORT         port of the temporary server (default 18398)
#   ZF_DRILL_SOURCE_DB    the database to back up (default zf_drill_src_<time>)
#   ZF_DRILL_TARGET_DB    the database to restore into (default zf_drill_dst_<time>)
set -euo pipefail

SCRATCH_PREFIX="zf_drill_"
STAMP="$(date +%s)"
SOURCE_DB="${ZF_DRILL_SOURCE_DB:-${SCRATCH_PREFIX}src_${STAMP}}"
TARGET_DB="${ZF_DRILL_TARGET_DB:-${SCRATCH_PREFIX}dst_${STAMP}}"

require_scratch() {
  case "$1" in
    "${SCRATCH_PREFIX}"?*) ;;
    *)
      echo "refusing to touch database '$1': a drill database must be named ${SCRATCH_PREFIX}*" >&2
      exit 2
      ;;
  esac
  if [[ ! "$1" =~ ^[a-z0-9_]+$ ]]; then
    echo "refusing database name '$1': use lowercase letters, digits and underscores" >&2
    exit 2
  fi
}

require_scratch "$SOURCE_DB"
require_scratch "$TARGET_DB"
if [[ "$SOURCE_DB" == "$TARGET_DB" ]]; then
  echo "refusing to restore a database into itself" >&2
  exit 2
fi

# A base names a server, never a database: scheme://authority and nothing after it. Anything
# else (a path, a query string, a fragment) could point the tools at another database than the
# one the script means, so it is refused. Prints the scheme and the authority, one per line.
split_base() {
  if [[ "$1" =~ ^(postgres|postgresql)://([^/?#]+)$ ]]; then
    printf '%s\n%s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
  else
    echo "refusing server URL: it must be scheme://user:password@host:port with no database, path or query string" >&2
    exit 2
  fi
}

# The URL of one database, built from the parts of its base.
url_of() {
  local parts
  mapfile -t parts < <(split_base "$1")
  printf '%s://%s/%s' "${parts[0]}" "${parts[1]}" "$2"
}

APP_BASE="${ZF_DRILL_APP_BASE:-postgres://postgres:postgres@127.0.0.1:5432}"
TOOL_BASE="${ZF_DRILL_TOOL_BASE:-$APP_BASE}"
split_base "$APP_BASE" >/dev/null
split_base "$TOOL_BASE" >/dev/null
PORT="${ZF_DRILL_PORT:-18398}"
read -r -a PG_PREFIX <<<"${ZF_DRILL_PG_PREFIX:-}"
if [[ -n "${ZF_DRILL_BIN:-}" ]]; then
  BIN="$ZF_DRILL_BIN"
elif [[ -x target/debug/zirv-flow.exe ]]; then
  BIN=target/debug/zirv-flow.exe
else
  BIN=target/debug/zirv-flow
fi

for tool in curl jq; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 2; }
done
[[ -x "$BIN" ]] || { echo "build the binary first (cargo build); not found: $BIN" >&2; exit 2; }

pg() { "${PG_PREFIX[@]}" "$@"; }
sql() { pg psql -X -q -At "$(url_of "$TOOL_BASE" "$1")" -c "$2"; }

native() {
  if command -v cygpath >/dev/null; then cygpath -m "$1"; else printf '%s' "$1"; fi
}

WORK="$(mktemp -d)"
LEDGER="$WORK/ledger"
mkdir -p "$LEDGER"
JAR="$WORK/cookies"
CREATED=()
SERVER_PID=""
CSRF=""

SERVER_STARTED=0

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; }

wait_port_free() {
  local i
  for i in $(seq 1 40); do
    if ! port_in_use; then return 0; fi
    sleep 0.5
  done
  echo "port $PORT is still in use after the server was stopped" >&2
  return 1
}

# Stops the server this script started and no other process, then checks that its port is free.
# Under MSYS and Cygwin the background job is an MSYS process whose native image has another
# (Windows) PID, read from /proc/<pid>/winpid; it is stopped with its own children only when the
# image name is the drill binary's.
stop_server() {
  if [[ -n "$SERVER_PID" ]]; then
    if [[ -r "/proc/$SERVER_PID/winpid" ]]; then
      local winpid image
      winpid="$(<"/proc/$SERVER_PID/winpid")"
      image="$(basename "$BIN")"
      if tasklist //FI "PID eq $winpid" //FO CSV //NH 2>/dev/null | grep -qiF "\"$image\""; then
        taskkill //PID "$winpid" //T //F >/dev/null 2>&1 || true
      fi
    else
      kill "$SERVER_PID" 2>/dev/null || true
    fi
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
  fi
  if [[ "$SERVER_STARTED" -eq 1 ]]; then wait_port_free; fi
}

# Runs on every exit path: stops the server, verifies the port, drops the scratch databases.
cleanup() {
  local code=$?
  trap - EXIT
  stop_server || code=1
  local name
  for name in "${CREATED[@]}"; do
    require_scratch "$name"
    sql postgres "DROP DATABASE IF EXISTS \"$name\" WITH (FORCE)" >/dev/null 2>&1 || true
  done
  rm -rf "$WORK"
  exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

create_db() {
  require_scratch "$1"
  sql postgres "CREATE DATABASE \"$1\"" >/dev/null
  CREATED+=("$1")
}

# The binary's environment for one database, in ENV_ARGS.
ENV_ARGS=()
set_env() {
  ENV_ARGS=(
    "ZIRV_FLOW_DATABASE__URL=$(url_of "$APP_BASE" "$1")"
    "ZIRV_FLOW_SECRETS__MASTER_KEY_FILE=$(native "$WORK/master.key")"
    "ZIRV_FLOW_PRIVACY__LEDGER_DIR=$(native "$LEDGER")"
    "ZIRV_FLOW_SERVER__BIND=127.0.0.1:$PORT"
    "ZIRV_FLOW_SERVER__COOKIE_SECURE=false"
  )
}

run_bin() {
  local db="$1"
  shift
  set_env "$db"
  env "${ENV_ARGS[@]}" "$BIN" "$@"
}

start_server() {
  if port_in_use; then
    echo "refusing to start: port $PORT is already in use; set ZF_DRILL_PORT to a free port" >&2
    exit 2
  fi
  set_env "$1"
  # A plain background command, so that $! is the server itself and not a subshell around it.
  env "${ENV_ARGS[@]}" "$BIN" all >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  SERVER_STARTED=1
  local i
  for i in $(seq 1 120); do
    if curl -fsS "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  echo "the server never became ready; the last lines of its log:" >&2
  tail -n 5 "$WORK/server.log" >&2
  exit 1
}

# Prints the body, then the status code on its own line.
api() {
  local method="$1" path="$2" body="${3:-}"
  local args=(-sS -X "$method" -b "$JAR" -c "$JAR" -H "x-csrf-token: $CSRF"
    -H 'content-type: application/json' -w '\n%{http_code}')
  if [[ -n "$body" ]]; then args+=(-d "$body"); fi
  curl "${args[@]}" "http://127.0.0.1:$PORT/api/v1$path"
}

STEP_START=0
step() {
  STEP_START="$(date +%s)"
  printf '\n### %s\n\n' "$1"
}
took() { printf -- '- took %ss\n' "$(($(date +%s) - STEP_START))"; }

FAILED=0
PASSED=0
check() {
  if [[ "$2" == "$3" ]]; then
    PASSED=$((PASSED + 1))
    printf -- '- PASS %s (%s)\n' "$1" "$3"
  else
    FAILED=$((FAILED + 1))
    printf -- '- FAIL %s: expected %s, got %s\n' "$1" "$2" "$3"
  fi
}

wait_for() {
  local db="$1" query="$2" want="$3" i
  for i in $(seq 1 120); do
    if [[ "$(sql "$db" "$query")" == "$want" ]]; then return 0; fi
    sleep 0.5
  done
  echo "timed out waiting for: $query = $want" >&2
  exit 1
}

TOTAL_START="$(date +%s)"
printf '# Restore drill\n\n'
printf -- '- zirv-flow: %s\n' "$("$BIN" --version)"
printf -- '- PostgreSQL server: %s\n' "$(sql postgres 'SHOW server_version')"
printf -- '- pg_dump: %s\n' "$(pg pg_dump --version)"
printf -- '- pg_restore: %s\n' "$(pg pg_restore --version)"

step "1. Prepare the source database"
"$BIN" admin generate-master-key --out "$(native "$WORK/master.key")" >/dev/null
create_db "$SOURCE_DB"
run_bin "$SOURCE_DB" migrate >/dev/null
printf 'zf-drill-%s-passphrase\n' "$STAMP" |
  run_bin "$SOURCE_DB" admin create-owner --email drill-owner@example.com >/dev/null
start_server "$SOURCE_DB"
LOGIN="$(curl -fsS -c "$JAR" -H 'content-type: application/json' \
  -d "{\"email\":\"drill-owner@example.com\",\"password\":\"zf-drill-$STAMP-passphrase\"}" \
  "http://127.0.0.1:$PORT/api/v1/session")"
CSRF="$(jq -r .csrf_token <<<"$LOGIN")"
WS="$(api POST /workspaces '{"name":"Drill"}' | sed '$d' | jq -r .id)"
KEY="$(api POST "/workspaces/$WS/api-keys" '{"name":"drill","scopes":["ingest"]}' | sed '$d' | jq -r .key)"
for who in one two three; do
  curl -fsS -o /dev/null -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
    -d "{\"identifiers\":{\"email\":\"$who@example.com\"},\"traits\":{\"plan\":\"free\"}}" \
    "http://127.0.0.1:$PORT/api/v1/ingest/identify"
done
wait_for "$SOURCE_DB" "SELECT count(*) FROM people" 3
check "people in the source before the backup" 3 "$(sql "$SOURCE_DB" 'SELECT count(*) FROM people')"
took

step "2. Back up with pg_dump"
stop_server
pg pg_dump -Fc "$(url_of "$TOOL_BASE" "$SOURCE_DB")" >"$WORK/backup.dump"
check "the dump is not empty" yes "$([[ -s "$WORK/backup.dump" ]] && echo yes || echo no)"
took

step "3. Erase one person after the backup"
start_server "$SOURCE_DB"
ERASED="$(sql "$SOURCE_DB" "SELECT person_id FROM identities WHERE kind = 'email' AND value = 'two@example.com'")"
REPLY="$(api POST "/workspaces/$WS/privacy/erasures" "{\"person_id\":\"$ERASED\"}")"
check "the erasure request is accepted" 202 "$(tail -n 1 <<<"$REPLY")"
wait_for "$SOURCE_DB" "SELECT count(*) FROM erasure_requests WHERE status = 'completed'" 1
stop_server
check "ledger files written" 1 "$(find "$LEDGER" -name '*.json' | wc -l | tr -d ' ')"
check "people in the source after the erasure" 2 "$(sql "$SOURCE_DB" 'SELECT count(*) FROM people')"
took

step "4. Restore with pg_restore into a fresh database"
create_db "$TARGET_DB"
pg pg_restore --no-owner --exit-on-error -d "$(url_of "$TOOL_BASE" "$TARGET_DB")" <"$WORK/backup.dump"
check "people in the restored database" 3 "$(sql "$TARGET_DB" 'SELECT count(*) FROM people')"
check "the erasure is missing from the restored ledger" 0 "$(sql "$TARGET_DB" 'SELECT count(*) FROM erasure_requests')"
took

step "5. ANALYZE, then replay the erasures from the ledger directory"
sql "$TARGET_DB" "ANALYZE" >/dev/null
REPLAY="$(run_bin "$TARGET_DB" admin replay-erasures --from "$(native "$LEDGER")")"
printf -- '- replay-erasures output: `%s`\n' "$(tr '\n' ' ' <<<"$REPLAY" | sed 's/ *$//')"
took

step "6. Assertions through SQL"
check "the erased person is gone" 0 "$(sql "$TARGET_DB" "SELECT count(*) FROM people WHERE id = '$ERASED'")"
check "the erased person's identities are gone" 0 "$(sql "$TARGET_DB" "SELECT count(*) FROM identities WHERE person_id = '$ERASED'")"
check "erased suppressions" 1 "$(sql "$TARGET_DB" "SELECT count(*) FROM suppressions WHERE reason = 'erased'")"
check "completed ledger entries" 1 "$(sql "$TARGET_DB" "SELECT count(*) FROM erasure_requests WHERE status = 'completed'")"
check "the other people are intact" 2 "$(sql "$TARGET_DB" 'SELECT count(*) FROM people')"
check "their identities are intact" 2 "$(sql "$TARGET_DB" 'SELECT count(*) FROM identities')"
took

step "7. Assertions through the API"
start_server "$TARGET_DB"
check "/readyz" 200 "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/readyz")"
SURVIVOR="$(sql "$TARGET_DB" "SELECT person_id FROM identities WHERE kind = 'email' AND value = 'one@example.com'")"
check "the erased person is 404" 404 "$(api GET "/workspaces/$WS/people/$ERASED" | tail -n 1)"
check "a surviving person is 200" 200 "$(api GET "/workspaces/$WS/people/$SURVIVOR" | tail -n 1)"
stop_server
took

printf '\n### Result\n\n'
printf -- '- assertions passed: %s, failed: %s\n' "$PASSED" "$FAILED"
printf -- '- total duration: %ss\n' "$(($(date +%s) - TOTAL_START))"
printf -- '- scratch databases created and dropped: %s\n' "${#CREATED[@]}"
[[ "$FAILED" -eq 0 ]]
