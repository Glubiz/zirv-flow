#!/usr/bin/env bash
# CI only: the actual quickstart plus DNS, TLS SMTP and an HTTPS front door.
set +x
set -euo pipefail
umask 077
[[ ${CI:-} == true ]] || { echo 'Run the getting-started walkthrough in CI only.' >&2; exit 1; }
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=scripts/drill/lib.sh
source "$ROOT/scripts/drill/lib.sh"
export DRILL_WORK DRILL_IMAGE
DRILL_WORK=$(mktemp -d)
DRILL_IMAGE="ghcr.io/glubiz/zirv-flow:${IMAGE_TAG:-upgrade-drill}"
PROJECT="zf-guide-${GITHUB_RUN_ID:?}-${GITHUB_RUN_ATTEMPT:?}"
COMPOSE_FILES=(-f "$ROOT/docker-compose.yml" -f "$ROOT/scripts/drill/compose.guide.yaml")
STARTED=true
CLEANUP_DIAGNOSTICS=before
CLEANUP_ERROR='::error::Getting-started walkthrough cleanup failed.'
cleanup_diagnostics() {
  echo '::error::Getting-started walkthrough failed; redacted service diagnostics follow.' >&2
  { cat "$DRILL_WORK/compose.log"; compose logs --no-color --tail 80 zirv-flow https fixture; } 2>&1 \
    | python3 -I "$ROOT/scripts/drill/upgrade-diagnostics.py" >&2 || true
}
install_cleanup_traps

fixture_tls guide-ca DNS:fixture,DNS:localhost

# The project and its volumes are disposable, including on an explicit rerun.
quiet_compose down -v --remove-orphans
quiet_compose up -d --no-build --wait --wait-timeout 120 postgres fixture
wait_fixture 'DNS/SMTP fixture did not start.'
quiet_compose up -d --no-build --wait --wait-timeout 120 zirv-flow
quiet_compose up -d --no-build --wait --wait-timeout 60 https
curl -fsS --retry 30 --retry-connrefused --retry-delay 1 --cacert "$DRILL_WORK/ca.pem" \
  https://localhost:8443/readyz > /dev/null
setup_token > "$DRILL_WORK/setup-token"
[[ -s "$DRILL_WORK/setup-token" ]] || { echo 'No setup token on the clean stack.' >&2; exit 1; }

cd "$ROOT/web"
npm exec -- playwright test -c e2e/guide/playwright.config.ts
