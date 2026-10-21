# shellcheck shell=bash
# Shared by the scripts in scripts/bench/m5 (source it, do not run it). It sets the M5 defaults and
# then sources scripts/bench/m4/common.sh, which provides the compose project, the database shell
# (pg), the API calls with an admin session (login, api, api_json, timed_post), stats, log and die.
#
# Environment (all optional; the M4_* names are the M4 scripts', kept so m4/common.sh works as is):
#   M4_PROJECT      compose project, default zirv-flow-m4bench10m (the 10 M-people database of M4)
#   M4_STATE        git-ignored directory with the credentials and state, default the M4 10 M state
#                   directory under the main checkout's .zirv/work. It holds the owner password, the
#                   session headers and the loadgen key: never commit or print its files.
#   M4_OUT          where results go, default <main checkout>/.zirv/work/b6079583-2324-4633-b995-e89d36016a57/m5-out
#   BENCH_IMAGE_TAG release tag of ghcr.io/glubiz/zirv-flow, default 0.76.0
#
# Needs bash, docker compose, jq, curl and awk. psql runs inside the postgres container.

m5_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
m5_root="$(cd "$m5_dir/../../.." && pwd)"
# The scripts may run from a linked worktree (<main>/.zirv/worktrees/<name>); the state and the
# work folder live in the main checkout.
case "$m5_root" in
  */.zirv/worktrees/*) m5_main="${m5_root%/.zirv/worktrees/*}" ;;
  *) m5_main="$m5_root" ;;
esac
export M4_PROJECT="${M4_PROJECT:-zirv-flow-m4bench10m}"
export M4_STATE="${M4_STATE:-$m5_main/.zirv/work/78780329-2a06-4f61-835f-0bdad59d6dd2/m4-state-10m}"
export M4_OUT="${M4_OUT:-$m5_main/.zirv/work/b6079583-2324-4633-b995-e89d36016a57/m5-out}"
export BENCH_IMAGE_TAG="${BENCH_IMAGE_TAG:-0.76.0}"

# shellcheck source=../m4/common.sh
. "$m5_dir/../m4/common.sh"

# Waits until no cargo.exe is running (another build or test run would skew a measurement).
wait_no_cargo() {
  until ! tasklist //FI "IMAGENAME eq cargo.exe" 2>/dev/null | grep -qi cargo.exe; do
    log "cargo.exe is running; waiting 30 s"
    sleep 30
  done
}
