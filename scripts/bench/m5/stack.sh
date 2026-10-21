#!/usr/bin/env bash
# The M5 benchmark stack (issue #169): the M4 stack script on the M5 defaults, so it brings up
# the compose project zirv-flow-m4bench10m (10 M people) on the published image 0.76.0.
# `reset` is not offered here: this project's volumes are never removed.
#
# Usage: scripts/bench/m5/stack.sh up|stop|down|status
#   up      start the stack on its volumes, sign in, write $M4_OUT/stack.txt
#   stop    stop the containers and keep the volumes
#   down    remove the containers and keep the volumes (not used by the M5 runs)
#   status  compose ps

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
case "${1:-}" in
  up | stop | down | status) exec "$m4_dir/stack.sh" "$1" ;;
  *) echo "usage: stack.sh up|stop|down|status" >&2; exit 2 ;;
esac
