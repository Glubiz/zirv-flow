#!/usr/bin/env bash
# The M6 send bench stack (#233): the M4 stack script in the compose project zirv-flow-m4bench10m
# with the M6 environment layer (egress allowlist for the host sink, render pool size, public
# URL, worker ports 9110-9119). Volumes are never removed. The image must be pulled first, and
# BENCH_IMAGE_TAG must name it.
#
# Usage: scripts/bench/m6/stack.sh up|stop|down|status
#   up      start serve, the scheduler, ONE worker and PostgreSQL on the existing volumes
#   stop    stop the containers, keep the volumes
#   down    remove the containers, keep the volumes
#   status  compose ps

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
case "${1:-}" in
  up)
    [ -z "${BENCH_IMAGE_TAG_UNSET:-}" ] || die "set BENCH_IMAGE_TAG to the pulled image tag"
    exec "$m6_dir/../m4/stack.sh" up ;;
  stop | down | status) exec "$m6_dir/../m4/stack.sh" "$1" ;;
  *) echo "usage: stack.sh up|stop|down|status" >&2; exit 2 ;;
esac
