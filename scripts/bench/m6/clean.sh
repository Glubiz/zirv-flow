#!/usr/bin/env bash
# Removes the M6 bench workspace and everything it owns (people, messages, flows, jobs), as the
# issue asks, once the runs are done. It is the one delete in these scripts, scoped to the
# workspace id in the state file (its API keys and memberships first: those two foreign keys do
# not cascade), so it asks for M6_CONFIRM_CLEAN=1. The sink stops and the
# workers go back to one process. Volumes are not touched.
#
# Usage: M6_CONFIRM_CLEAN=1 scripts/bench/m6/clean.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
[ "${M6_CONFIRM_CLEAN:-0}" = 1 ] || die "this deletes workspace $(state_get M6_WS); set M6_CONFIRM_CLEAN=1"
ws="$(m6_ws)"
"$m6_dir/sink.sh" stop
compose up -d --no-build --no-deps --scale zirv-flow-worker=1 zirv-flow-worker >&2 || true
pgc -v ws="$ws" -c "SET max_parallel_workers_per_gather = 0; SET statement_timeout = 0; DELETE FROM api_keys WHERE workspace_id = :'ws'; DELETE FROM memberships WHERE workspace_id = :'ws'; DELETE FROM workspaces WHERE id = :'ws'"
for key in M6_WS M6_DOMAIN_ID M6_IDENTITY M6_CRED M6_DEST M6_TPL_BLOCKS M6_TPL_HTML; do state_put "$key" ""; done
rm -f "$M4_STATE/m6-ingest.key"
log "workspace $ws deleted"
