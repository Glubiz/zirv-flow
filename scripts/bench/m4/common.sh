# shellcheck shell=bash
# Shared by the scripts in scripts/bench/m4 (source it, do not run it): the compose project, the
# state directory, the database shell and the API calls with an admin session.
#
# Environment (all optional):
#   M4_PROJECT      compose project, default zirv-flow-m4bench (never the quickstart's or the
#                   M2 bench's `zirv-flow-bench`)
#   M4_STATE        git-ignored directory for credentials and state, default
#                   <repo>/.zirv/work/78780329-2a06-4f61-835f-0bdad59d6dd2/m4-state. It holds
#                   the owner password, the session header file and the loadgen key: never
#                   commit it and never print its files.
#   M4_OUT          where measurement results go, default $M4_STATE/out
#   M4_HOST, M4_PORT  the API, default 127.0.0.1:8080
#   M4_EMAIL        the bench owner, default bench@example.com
#   M4_EXTRA_COMPOSE  one more compose file layered last (the M6 send bench adds its environment)
#   M4_RESOURCE_COMPOSE replaces the tier and M4 image layers (M8 supplies its resource layer)
#   M4_TIER         app tier S, M or L (compose.tiers.yaml), default M: 4 vCPU, worker concurrency 4
#   BENCH_IMAGE_TAG release tag of ghcr.io/glubiz/zirv-flow, default 0.55.0
#   BENCH_PG_CPUSET, BENCH_PG_SHM  PostgreSQL cpuset (default 8-31) and /dev/shm (default 1gb)
#
# Needs bash, docker compose, jq, curl and awk. psql runs inside the postgres container.

set -euo pipefail

m4_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$m4_dir/../../.." && pwd)"
cd "$root"

M4_PROJECT="${M4_PROJECT:-zirv-flow-m4bench}"
M4_STATE="${M4_STATE:-$root/.zirv/work/78780329-2a06-4f61-835f-0bdad59d6dd2/m4-state}"
M4_OUT="${M4_OUT:-$M4_STATE/out}"
M4_EMAIL="${M4_EMAIL:-bench@example.com}"
M4_TIER="${M4_TIER:-M}"
BENCH_IMAGE_TAG="${BENCH_IMAGE_TAG:-0.55.0}"
api_base="http://${M4_HOST:-127.0.0.1}:${M4_PORT:-8080}"
mkdir -p "$M4_STATE" "$M4_OUT"
chmod 700 "$M4_STATE" 2>/dev/null || true
printf '*\n' >"$M4_STATE/.gitignore"

case "$M4_TIER" in
  S) BENCH_APP_CPUSET="0-1"; mem=(2048 1638 410) ;;
  M) BENCH_APP_CPUSET="0-3"; mem=(4096 3277 819) ;;
  L) BENCH_APP_CPUSET="0-7"; mem=(8192 6554 1638) ;;
  *) echo "M4_TIER must be S, M or L" >&2; exit 2 ;;
esac
export BENCH_APP_CPUSET BENCH_IMAGE_TAG
export BENCH_MEM_WORKER="${mem[0]}m" BENCH_MEM_SERVE="${mem[1]}m" BENCH_MEM_SCHEDULER="${mem[2]}m"
export BENCH_PG_SHM="${BENCH_PG_SHM:-1gb}"

# A decimal-comma locale would make awk print 0,121 into JSON.
export LC_NUMERIC=C

# BSD date (macOS) lacks %6N and -d; emulate the forms the bench scripts use. GNU date is untouched.
if ! [[ "$(date +%6N)" =~ ^[0-9]+$ ]]; then
  date() {
    case "$*" in
      '+%s%6N') python3 -I -c 'import time; print(time.time_ns() // 1000)' ;;
      '+%s.%N') python3 -I -c 'import time; print(f"{time.time():.9f}")' ;;
      '-u +%FT%T.%6NZ') python3 -I -c 'import datetime as d; print(d.datetime.now(d.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"))' ;;
      "-u -d +3 days +%a") python3 -I -c 'import datetime as d; print((d.datetime.now(d.timezone.utc) + d.timedelta(days=3)).strftime("%a"))' ;;
      *) command date "$@" ;;
    esac
  }
fi
log() { echo "[m4 $(date -u +%T)] $*" >&2; }
die() { log "FAILED: $*"; exit 1; }

# The project name given with -p wins over the `name:` in compose.bench.yaml.
compose() {
  local resources=(-f scripts/bench/compose.tiers.yaml -f scripts/bench/m4/compose.m4.yaml)
  [ -z "${M4_RESOURCE_COMPOSE:-}" ] || resources=(-f "$M4_RESOURCE_COMPOSE")
  docker compose -p "$M4_PROJECT" -f docker-compose.yml -f scripts/bench/compose.bench.yaml \
    "${resources[@]}" \
    ${M4_EXTRA_COMPOSE:+-f "$M4_EXTRA_COMPOSE"} "$@"
}

pg_container() { compose ps -q postgres; }

# psql inside the postgres container. Extra arguments go to psql; the script comes on stdin
# (-f -) or in -c. The role and database are the compose defaults.
pg() { docker exec -i "$(pg_container)" psql -U "${POSTGRES_USER:-zirv}" -d "${POSTGRES_DB:-zirv_flow}" -X -q -v ON_ERROR_STOP=1 "$@"; }

state_get() { [ -f "$M4_STATE/state.env" ] && sed -n "s/^$1=//p" "$M4_STATE/state.env" | tail -n 1 || true; }
state_put() {
  touch "$M4_STATE/state.env"
  grep -v "^$1=" "$M4_STATE/state.env" >"$M4_STATE/state.env.tmp" || true
  echo "$1=$2" >>"$M4_STATE/state.env.tmp"
  mv "$M4_STATE/state.env.tmp" "$M4_STATE/state.env"
}

# login: signs in as the bench owner and writes the session and CSRF headers to
# $M4_STATE/auth.hdr (read by curl with -H @file, so no secret is ever on a command line).
login() {
  [ -f "$M4_STATE/owner.pass" ] || die "no owner password: run stack.sh up first"
  local headers body csrf session
  headers="$(mktemp)"
  body="$(jq -n --arg e "$M4_EMAIL" --rawfile p "$M4_STATE/owner.pass" \
    '{email: $e, password: ($p | rtrimstr("\n"))}' \
    | curl -sS -D "$headers" -X POST "$api_base/api/v1/session" -H 'content-type: application/json' \
        --data-binary @- -w '\n%{http_code}')" || { rm -f "$headers"; return 1; }
  if [ "$(tail -n 1 <<<"$body")" != 200 ]; then rm -f "$headers"; return 1; fi
  csrf="$(sed '$d' <<<"$body" | jq -r .csrf_token)"
  session="$(tr -d '\r' <"$headers" | sed -nE 's/^[Ss]et-[Cc]ookie: (zf_session=[^;]+).*/\1/p' | head -n1)"
  rm -f "$headers"
  [ -n "$csrf" ] && [ -n "$session" ] || return 1
  ( umask 077; printf 'cookie: %s\nx-csrf-token: %s\n' "$session" "$csrf" >"$M4_STATE/auth.hdr" )
}

# api METHOD PATH [BODY_FILE]: the JSON body on standard output; exit status 1 and the problem
# on standard error for an HTTP status of 400 or more. PATH is below /api/v1.
api() {
  local method="$1" path="$2" data="${3:-}" out status body=()
  out="$(mktemp)"
  [ -z "$data" ] || body=(--data-binary "@$data")
  status="$(curl -sS -o "$out" -w '%{http_code}' -X "$method" "$api_base/api/v1$path" \
    -H "@$M4_STATE/auth.hdr" -H 'content-type: application/json' ${body[@]+"${body[@]}"})"
  if [ "$status" -ge 400 ]; then
    echo "$method $path -> $status: $(head -c 600 "$out")" >&2
    rm -f "$out"
    return 1
  fi
  cat "$out"
  rm -f "$out"
}

# api_json METHOD PATH JSON: the same with the body given as text.
api_json() {
  local tmp rc=0
  tmp="$(mktemp)"
  printf '%s' "$3" >"$tmp"
  api "$1" "$2" "$tmp" || rc=$?
  rm -f "$tmp"
  return "$rc"
}

# timed_post PATH BODY_FILE OUT_FILE: POST, the response body into OUT_FILE, and "status seconds"
# on standard output (curl's total time, which includes connecting).
timed_post() {
  curl -sS -o "$3" -w '%{http_code} %{time_total}' -X POST "$api_base/api/v1$1" \
    -H "@$M4_STATE/auth.hdr" -H 'content-type: application/json' --data-binary "@$2"
}

# stats: reads seconds, one per line, on standard input and prints a JSON object with the
# count, min, max, the median and the nearest-rank p95 (the ceil(0.95 n)-th smallest value).
stats() {
  jq -sc '(map(select(. != null)) | sort) as $s
    | if ($s | length) == 0 then {n: 0}
      else {n: ($s | length), min: $s[0], max: $s[-1],
            p50: (($s | length) as $n | if $n % 2 == 1 then $s[($n - 1) / 2] else ($s[$n / 2 - 1] + $s[$n / 2]) / 2 end),
            p95: $s[(((($s | length) * 0.95) | ceil) - 1)]} end'
}
