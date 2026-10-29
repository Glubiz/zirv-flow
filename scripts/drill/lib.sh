# shellcheck shell=bash
# Sourced helpers; callers provide the Compose files, project and cleanup diagnostics.

compose() { docker compose -p "$PROJECT" "${COMPOSE_FILES[@]}" "$@"; }
quiet_compose() { compose "$@" > "$DRILL_WORK/compose.log" 2>&1; }

cleanup() {
  local status=$?
  trap - EXIT
  if [[ "$status" != 0 && "$CLEANUP_DIAGNOSTICS" == before ]]; then cleanup_diagnostics; fi
  if [[ "$STARTED" == true ]]; then
    if ! quiet_compose down -v --remove-orphans; then
      echo "$CLEANUP_ERROR" >&2
      status=1
    fi
  fi
  if [[ "$status" != 0 && "$CLEANUP_DIAGNOSTICS" == after ]]; then cleanup_diagnostics; fi
  rm -rf "$DRILL_WORK"
  exit "$status"
}

install_cleanup_traps() {
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
}

fixture_tls() {
  printf '{}\n' > "$DRILL_WORK/records.json"
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=$1" \
    -addext basicConstraints=critical,CA:TRUE -keyout "$DRILL_WORK/ca.key" \
    -out "$DRILL_WORK/ca.pem" > "$DRILL_WORK/tls.log" 2>&1
  openssl req -newkey rsa:2048 -nodes -subj /CN=fixture \
    -keyout "$DRILL_WORK/smtp.key" -out "$DRILL_WORK/smtp.csr" >> "$DRILL_WORK/tls.log" 2>&1
  printf '%s\n' "subjectAltName=$2" 'basicConstraints=critical,CA:FALSE' \
    'extendedKeyUsage=serverAuth' > "$DRILL_WORK/smtp.ext"
  openssl x509 -req -days 1 -in "$DRILL_WORK/smtp.csr" -CA "$DRILL_WORK/ca.pem" \
    -CAkey "$DRILL_WORK/ca.key" -CAcreateserial -extfile "$DRILL_WORK/smtp.ext" \
    -out "$DRILL_WORK/smtp.pem" >> "$DRILL_WORK/tls.log" 2>&1
}

wait_fixture() {
  DRILL_DNS_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$(compose ps -q fixture)")
  export DRILL_DNS_IP
  for _ in $(seq 1 60); do [[ ! -f "$DRILL_WORK/fixture-ready" ]] || break; sleep 1; done
  [[ -f "$DRILL_WORK/fixture-ready" ]] || { echo "$1" >&2; exit 1; }
}

setup_token() {
  compose logs --no-color zirv-flow | sed -nE 's/.*setup token ([^ ,"]+).*/\1/p' | head -1
}
