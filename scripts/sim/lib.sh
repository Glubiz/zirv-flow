# shellcheck shell=bash
# Sourced helpers of the simulated production environment. The caller sets SIM_DIR and STATE.

die() { echo "sim: $*" >&2; exit 1; }
say() { echo "sim: $*"; }

# A path both bash and the Windows docker.exe accept: C:/a/b on Git Bash, unchanged elsewhere.
native_path() {
  if command -v cygpath > /dev/null 2>&1; then cygpath -m "$1"; else printf '%s\n' "$1"; fi
}

need_tools() {
  local tool
  for tool in docker curl jq openssl; do
    command -v "$tool" > /dev/null 2>&1 || die "missing required tool: $tool"
  done
  docker compose version > /dev/null 2>&1 || die 'docker compose is not available'
}

# compose <args>: every call goes through the one project; never another project's resources.
compose() {
  local env_files=(--env-file "$SIM_DIR/profiles/$(cat "$STATE/profile").env")
  [[ ! -f "$STATE/env" ]] || env_files+=(--env-file "$STATE/env")
  SIM_STATE="$STATE" docker compose -p "$SIM_PROJECT" -f "$SIM_DIR/compose.yaml" \
    --project-directory "$SIM_DIR" "${env_files[@]}" "$@"
}

# Substitutes a compose-interpolated value from the env files for scripts (profile first, then state).
env_value() {
  local name=$1 file
  for file in "$STATE/env" "$SIM_DIR/profiles/$(cat "$STATE/profile").env"; do
    [[ -f "$file" ]] || continue
    sed -nE "s/^$name=(.*)$/\1/p" "$file" | head -1
  done | head -1
}

# sim_tls: a CA and one leaf with the SANs every HTTPS and SMTP fixture answers to.
# The openssl commands are those of fixture_tls in scripts/drill/lib.sh, with a long lifetime and
# fixed file names (the drill function writes into its scratch directory for one day).
sim_tls() {
  local dir=$1 log=$1/tls.log
  mkdir -p "$dir"
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=zirv-flow-sim-ca" \
    -addext basicConstraints=critical,CA:TRUE -keyout "$dir/ca.key" -out "$dir/ca.pem" > "$log" 2>&1
  openssl req -newkey rsa:2048 -nodes -subj /CN=sim-fixtures \
    -keyout "$dir/leaf.key" -out "$dir/leaf.csr" >> "$log" 2>&1
  printf '%s\n' \
    'subjectAltName=DNS:app.sim.example.com,DNS:billing-api.sim.example.com,DNS:exports.sim.example.com,DNS:mailer-api.sim.example.com,DNS:receiver.sim.example.com,DNS:smtp.sim.example.com' \
    'basicConstraints=critical,CA:FALSE' 'extendedKeyUsage=serverAuth' > "$dir/leaf.ext"
  openssl x509 -req -days 3650 -in "$dir/leaf.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" \
    -CAcreateserial -extfile "$dir/leaf.ext" -out "$dir/leaf.pem" >> "$log" 2>&1
  # The containers run as other users; these keys protect nothing outside the sim.
  chmod 644 "$dir"/*.pem "$dir"/*.key
}

random_secret() { openssl rand -hex 16; }

# api_curl <method> <path> [json]: a call through the load balancer with the session in $STATE/cookies.
api() {
  local method=$1 path=$2 body=${3:-}
  local args=(-sS --max-time 30 -X "$method" -b "$STATE/cookies" -c "$STATE/cookies"
    -H "x-csrf-token: ${CSRF:-}" -H 'content-type: application/json')
  [[ -z "$body" ]] || args+=(--data-binary "$body")
  curl "${args[@]}" -w '\n%{http_code}' "http://localhost:8080/api/v1$path"
}

# wait_until <seconds> <description> <command...>: polls once a second.
wait_until() {
  local seconds=$1 what=$2 _
  shift 2
  for _ in $(seq 1 "$seconds"); do
    if "$@" > /dev/null 2>&1; then return 0; fi
    sleep 1
  done
  die "timed out after ${seconds}s waiting for $what"
}

# metric <service> <port> <name>: one counter of a fixture's /metrics, read from inside the network.
metric() {
  compose exec -T "$1" python -c "
import sys, urllib.request
for line in urllib.request.urlopen('http://127.0.0.1:$2/metrics', timeout=3).read().decode().splitlines():
    if line.startswith('$3'):
        print(line)
" 2> /dev/null
}

# call <method> <path> [json] [expected-status-regex]: prints the body, dies with the step on any
# other status. The path is under /api/v1. Needs STEP (a name for the failure message).
call() {
  local out code
  out=$(api "$1" "$2" "${3:-}") || die "${STEP:-api}: curl failed for $1 $2"
  code=${out##*$'\n'}
  if [[ ! $code =~ ^(${4:-2[0-9][0-9]})$ ]]; then
    die "${STEP:-api}: $1 $2 answered $code: $(printf '%s' "${out%$'\n'*}" | head -c 600)"
  fi
  printf '%s' "${out%$'\n'*}"
}

# sign_in: opens a session as the owner and sets CSRF and WS (the workspace id, once it exists).
sign_in() {
  local password session
  password=$(env_value SIM_OWNER_PASSWORD)
  : > "$STATE/cookies"
  CSRF=
  session=$(call POST /session "$(jq -n --arg p "$password" '{email:"owner@sim.example.com",password:$p}')")
  CSRF=$(jq -er .csrf_token <<< "$session")
  # shellcheck disable=SC2034  # read by the scripts that source this file
  OWNER_ID=$(jq -er .user.id <<< "$session")
  # shellcheck disable=SC2034
  [[ ! -f "$STATE/keys/workspace.id" ]] || WS=$(cat "$STATE/keys/workspace.id")
}

# The Windows jq prints CRLF line ends, which end up inside ids captured with $(...).
jq() { command jq "$@" | tr -d '\r'; }

# poll_jq <path> <jq-filter>: succeeds once the GET answer satisfies the filter (no dying).
poll_jq() {
  local out
  out=$(api GET "$1" 2> /dev/null) || return 1
  [[ ${out##*$'\n'} =~ ^2 ]] || return 1
  printf '%s' "${out%$'\n'*}" | command jq -e "$2" > /dev/null 2>&1
}
