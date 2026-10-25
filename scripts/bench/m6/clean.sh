#!/usr/bin/env bash
# Removes the M6 bench workspace and everything it owns (people, messages, flows, jobs), as the
# issue asks, once the runs are done. It is the one delete in these scripts, scoped to the
# workspace id in the state file, so it asks for M6_CONFIRM_CLEAN=1. The sink stops and the
# workers go back to one process. Volumes are not touched.
#
# Usage: M6_CONFIRM_CLEAN=1 scripts/bench/m6/clean.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
[ "${M6_CONFIRM_CLEAN:-0}" = 1 ] || die "this deletes workspace $(state_get M6_WS); set M6_CONFIRM_CLEAN=1"
ws="$(m6_ws)"
"$m6_dir/sink.sh" stop
compose up -d --no-build --no-deps --scale zirv-flow-worker=1 zirv-flow-worker >&2 || true
# The product has no workspace delete, and ON DELETE CASCADE from `workspaces` runs a foreign key
# check per deleted row (a cascading delete of the 70,000-person bench workspace ran for 21 minutes
# and was cancelled). So every table row of this one workspace is deleted set-based with the
# foreign key triggers off for the session (`session_replication_role = replica`, superuser), table
# by table, then the workspace row itself. Only rows with this workspace id are touched.
pg -v ws="$ws" -f - <<'SQL'
SET session_replication_role = replica;
SET max_parallel_workers_per_gather = 0;
SET statement_timeout = 0;
SELECT set_config('m6.ws', :'ws', false);
DO $$
DECLARE t text;
BEGIN
  FOR t IN
    SELECT c.relname FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
      JOIN pg_attribute a ON a.attrelid = c.oid AND a.attname = 'workspace_id' AND NOT a.attisdropped
     WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition AND c.relname <> 'workspaces'
  LOOP
    EXECUTE format('DELETE FROM %I WHERE workspace_id = %L', t, current_setting('m6.ws'));
  END LOOP;
  EXECUTE format('DELETE FROM workspaces WHERE id = %L', current_setting('m6.ws'));
END $$;
SQL
for key in M6_WS M6_DOMAIN_ID M6_IDENTITY M6_CRED M6_DEST M6_TPL_BLOCKS M6_TPL_HTML; do state_put "$key" ""; done
rm -f "$M4_STATE/m6-ingest.key"
log "workspace $ws deleted"
