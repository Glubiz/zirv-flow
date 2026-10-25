# shellcheck shell=bash
# Shared by the scripts in scripts/bench/m6 (source it, do not run it): the M5 defaults (compose
# project zirv-flow-m4bench10m, the state directory), the M6 compose layer and the send bench's
# own state keys. See m4/common.sh for the rest (pg, api, api_json, login, state_get, state_put).
#
# Environment (all optional):
#   BENCH_IMAGE_TAG    release tag of ghcr.io/glubiz/zirv-flow; required for stack.sh up
#   M6_OUT             results, default <main checkout>/.zirv/work/m6-send-out
#   M6_SINK_PORT       the sink's port on the host, default 2525
#   M6_SINK_HOST       the name containers use for the host, default host.docker.internal
#   M6_DOMAIN          the fixture sending domain, default bench-send.example.com
#   M6_RCPT_DOMAIN     the people's address domain, default bench.example.com
#   SENDBENCH          the sendbench binary, default <repo>/target/release/sendbench
#
# Needs bash, docker compose, jq, curl and awk, as the M4 scripts do.

m6_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
m6_root="$(cd "$m6_dir/../../.." && pwd)"
case "$m6_root" in
  */.zirv/worktrees/*) m6_main="${m6_root%/.zirv/worktrees/*}" ;;
  *) m6_main="$m6_root" ;;
esac
export M4_PROJECT="${M4_PROJECT:-zirv-flow-m4bench10m}"
export M4_STATE="${M4_STATE:-$m6_main/.zirv/work/78780329-2a06-4f61-835f-0bdad59d6dd2/m4-state-10m}"
M6_OUT="${M6_OUT:-$m6_main/.zirv/work/m6-send-out}"
export M4_OUT="${M4_OUT:-$M6_OUT}"
export M4_EXTRA_COMPOSE="$m6_dir/compose.m6.yaml"
export BENCH_IMAGE_TAG="${BENCH_IMAGE_TAG:-}"

# shellcheck source=../m4/common.sh
BENCH_IMAGE_TAG_SET="$BENCH_IMAGE_TAG"
. "$m6_dir/../m4/common.sh"
# m4/common.sh defaults the tag; the send bench wants it named explicitly.
[ -n "$BENCH_IMAGE_TAG_SET" ] || BENCH_IMAGE_TAG_UNSET=1
M6_SINK_PORT="${M6_SINK_PORT:-2525}"
M6_SINK_HOST="${M6_SINK_HOST:-host.docker.internal}"
M6_DOMAIN="${M6_DOMAIN:-bench-send.example.com}"
M6_RCPT_DOMAIN="${M6_RCPT_DOMAIN:-bench.example.com}"
SENDBENCH="${SENDBENCH:-$m6_root/target/release/sendbench}"
M6_SINK_DIR="$M6_OUT/sink"
mkdir -p "$M6_OUT" "$M6_SINK_DIR"

m6_ws() { local ws; ws="$(state_get M6_WS)"; [ -n "$ws" ] || die "no M6 workspace: run setup.sh"; echo "$ws"; }

# pgc [psql args] -c SQL: like pg, but the SQL goes in on standard input, because psql does not
# expand :'var' variables in a -c string.
pgc() {
  local args=() sql=""
  while [ $# -gt 0 ]; do
    if [ "$1" = -c ]; then sql="$2"; shift 2; else args+=("$1"); shift; fi
  done
  printf '%s\n' "$sql" | pg "${args[@]}" -f -
}

# Waits until no cargo.exe is running (another build or test run would skew a measurement).
wait_no_cargo() {
  until ! tasklist //FI "IMAGENAME eq cargo.exe" 2>/dev/null | grep -qi cargo.exe; do
    log "cargo.exe is running; waiting 30 s"
    sleep 30
  done
}
