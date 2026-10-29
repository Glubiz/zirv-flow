#!/usr/bin/env bash
# CI only: the actual quickstart plus DNS, TLS SMTP and an HTTPS front door.
set +x
set -euo pipefail
umask 077
[[ ${CI:-} == true ]] || { echo 'Run the getting-started walkthrough in CI only.' >&2; exit 1; }
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
export DRILL_WORK DRILL_IMAGE
DRILL_WORK=$(mktemp -d)
DRILL_IMAGE="ghcr.io/glubiz/zirv-flow:${IMAGE_TAG:-upgrade-drill}"
PROJECT="zf-guide-${GITHUB_RUN_ID:?}-${GITHUB_RUN_ATTEMPT:?}"
compose() { docker compose -p "$PROJECT" -f "$ROOT/docker-compose.yml" -f "$ROOT/scripts/drill/compose.guide.yaml" "$@"; }
quiet_compose() { compose "$@" > "$DRILL_WORK/compose.log" 2>&1; }
cleanup() {
  local status=$?
  trap - EXIT
  if [[ $status != 0 ]]; then
    echo '::error::Getting-started walkthrough failed; redacted service diagnostics follow.' >&2
    { cat "$DRILL_WORK/compose.log"; compose logs --no-color --tail 80 zirv-flow https fixture; } 2>&1 \
      | python3 -I "$ROOT/scripts/drill/upgrade-diagnostics.py" >&2 || true
  fi
  if ! quiet_compose down -v --remove-orphans; then
    echo '::error::Getting-started walkthrough cleanup failed.' >&2
    status=1
  fi
  rm -rf "$DRILL_WORK"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf '{}\n' > "$DRILL_WORK/records.json"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=guide-ca \
  -addext basicConstraints=critical,CA:TRUE -keyout "$DRILL_WORK/ca.key" \
  -out "$DRILL_WORK/ca.pem" > "$DRILL_WORK/tls.log" 2>&1
openssl req -newkey rsa:2048 -nodes -subj /CN=fixture \
  -keyout "$DRILL_WORK/smtp.key" -out "$DRILL_WORK/smtp.csr" >> "$DRILL_WORK/tls.log" 2>&1
printf '%s\n' 'subjectAltName=DNS:fixture,DNS:localhost' 'basicConstraints=critical,CA:FALSE' \
  'extendedKeyUsage=serverAuth' > "$DRILL_WORK/smtp.ext"
openssl x509 -req -days 1 -in "$DRILL_WORK/smtp.csr" -CA "$DRILL_WORK/ca.pem" \
  -CAkey "$DRILL_WORK/ca.key" -CAcreateserial -extfile "$DRILL_WORK/smtp.ext" \
  -out "$DRILL_WORK/smtp.pem" >> "$DRILL_WORK/tls.log" 2>&1

# The project and its volumes are disposable, including on an explicit rerun.
quiet_compose down -v --remove-orphans
quiet_compose up -d --no-build --wait --wait-timeout 120 postgres fixture
DRILL_DNS_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$(compose ps -q fixture)")
export DRILL_DNS_IP
for _ in $(seq 1 60); do [[ ! -f "$DRILL_WORK/fixture-ready" ]] || break; sleep 1; done
[[ -f "$DRILL_WORK/fixture-ready" ]] || { echo 'DNS/SMTP fixture did not start.' >&2; exit 1; }
quiet_compose up -d --no-build --wait --wait-timeout 120 zirv-flow
quiet_compose up -d --no-build --wait --wait-timeout 60 https
curl -fsS --retry 30 --retry-connrefused --retry-delay 1 --cacert "$DRILL_WORK/ca.pem" \
  https://localhost:8443/readyz > /dev/null
compose logs --no-color zirv-flow \
  | sed -nE 's/.*setup token ([^ ,"]+).*/\1/p' | head -1 > "$DRILL_WORK/setup-token"
[[ -s "$DRILL_WORK/setup-token" ]] || { echo 'No setup token on the clean stack.' >&2; exit 1; }

cd "$ROOT/web"
npm exec -- playwright test -c e2e/guide/playwright.config.ts
