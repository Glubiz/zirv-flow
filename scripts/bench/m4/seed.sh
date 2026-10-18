#!/usr/bin/env bash
# Seeds the M4 bench database (issue #127, spec D3) by SQL into the real tables, after the app
# has migrated it (stack.sh up) and setup.sh has created the workspace, the trait definitions and
# the event schemas through the API. It runs seed.sql, then VACUUM (ANALYZE) on the seeded
# tables, and writes $M4_OUT/seed.json: the row counts, the on-disk size of each table and index
# group, the seed instant and the PostgreSQL version.
#
# What the seed reproduces, and where each fact was confirmed (paths below crates/):
#   people             zirv-flow-store/migrations/0006_people.sql:7-16 (id, workspace_id, traits,
#                      trait_sources, merged_into, created_at, updated_at) and 0033_trait_slots.sql
#                      (30 slot columns, NULL until a backfill); 0012_people_timeline.sql:12 (the
#                      GIN jsonb_path_ops index on traits stays and is maintained by the inserts)
#   people.traits      zirv-flow-core/src/people/traits.rs:142-187 (coerce: JSON string, number,
#                      bool; timestamp as RFC 3339 with Z via SecondsFormat::AutoSi; date as
#                      YYYY-MM-DD; list of strings); zirv-flow-store/src/people/apply.rs:963 (the
#                      UPDATE writes traits and trait_sources, a source name per key)
#   trait_definitions  created over the API by setup.sh (0006_people.sql:46-60 and 0033: slot,
#                      slot_state, slot_generation are NULL/NULL/0 until `POST .../index`)
#   identities         0006_people.sql:20-30, kind `external:user_id` for the userId loadgen sends
#                      (zirv-flow-api/src/ingest/payload.rs:247); only the loadgen pool, because
#                      the segment code reads identities only for the preview sample's display
#                      (zirv-flow-store/src/segments/preview.rs, strongest_identifiers)
#   events             0008_events.sql:10-27 (all columns), 0025_sync_deletion.sql:9
#                      (source_record_id, NULL), 0016_investigation.sql:44 and 0025:10 (the other
#                      indexes, kept); properties as the ingest path stores them; the segment
#                      compiler reads e.workspace_id, e.person_id, e.name, e.occurred_at and
#                      e.properties (zirv-flow-segments/src/lower.rs:629-715)
#   event schemas      setup.sh publishes version 1 of the five names; the catalog reads each
#                      name's current version (zirv-flow-store/src/segments/catalog.rs:54-62)
#
# Environment (see common.sh for the rest):
#   SEED_PEOPLE    people, default 100000 (the smoke run); the full run is 10000000
#   SEED_EVENTS    events, default 500000 (smoke); the full run is 50000000
#   SEED_LG_POOL   people with a loadgen identity, default 100000; loadgen's --people must not
#                  exceed it
#   SEED_DAYS      event window in days, default 365
#   SEED_CHUNK     rows per statement, default 1000000
#
# It refuses to run when the workspace already has people. Local bench databases only.
#
# Usage: scripts/bench/m4/seed.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ws="$(state_get M4_WS)"
[ -n "$ws" ] || die "no workspace: run setup.sh first"
people="${SEED_PEOPLE:-100000}"
events="${SEED_EVENTS:-500000}"
pool="${SEED_LG_POOL:-100000}"
days="${SEED_DAYS:-365}"
chunk="${SEED_CHUNK:-1000000}"
case "$people$events$pool$days$chunk" in *[!0-9]* | "") die "the SEED_* values must be positive integers" ;; esac

steps=" ${SEED_STEPS:-load vacuum-people vacuum-identities vacuum-events manifest} "
has() { case "$steps" in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
if has load; then
existing="$(pg -At -c "SELECT count(*) FROM people WHERE workspace_id = '$ws'")"
[ "$existing" = 0 ] || die "workspace $ws already has $existing people; reset the stack volumes to re-seed"
fi
traits="$(pg -At -c "SELECT count(*) FROM trait_definitions WHERE workspace_id = '$ws'")"
[ "$traits" = 20 ] || die "expected the 20 trait definitions of setup.sh, found $traits"

# SEED_STEPS (default: all) splits a long seed into commands that each stay under an hour:
# (PARALLEL 0: a parallel vacuum with maintenance_work_mem=1GB needs more /dev/shm than the 1gb the stack gives.)
#   load  vacuum-people  vacuum-identities  vacuum-events  manifest
# After the load, the later steps need SEED_AT (the seed_at the load logged) and, for the
# manifest, SEED_DURATION_S (the load's duration, recorded in seed.json).
if has load; then
  seed_at="$(date -u +%FT%TZ)"
  log "seeding $people people and $events events into workspace $ws (seed_at $seed_at)"
  start="$(date +%s)"
  pg -v "ws=$ws" -v "people=$people" -v "events=$events" -v "lg_pool=$pool" -v "days=$days" \
    -v "seed_at=$seed_at" -v "chunk=$chunk" -f - <"$m4_dir/seed.sql" >&2
  SEED_DURATION_S=$(( $(date +%s) - start ))
  log "rows written in $SEED_DURATION_S s"
else
  seed_at="${SEED_AT:?set SEED_AT to the seed_at of the load}"
fi
for table in people identities events; do
  if has "vacuum-$table"; then
    start="$(date +%s)"
    pg -c "VACUUM (ANALYZE, PARALLEL 0) $table" >&2
    log "VACUUM (ANALYZE, PARALLEL 0) $table done in $(( $(date +%s) - start )) s"
  fi
done
has manifest || exit 0

pg -At -v "ws=$ws" -v "seed_at=$seed_at" -v "days=$days" -v "duration_s=${SEED_DURATION_S:-null}" -f - >"$M4_OUT/seed.json" <<'SQL'
SELECT json_build_object(
    'workspace', :'ws',
    'seed_at', :'seed_at',
    'load_duration_s', :duration_s,
    'days', :days,
    'setseed', 0.42,
    'postgres', current_setting('server_version'),
    'rows', json_build_object(
        'people', (SELECT count(*) FROM people WHERE workspace_id = :'ws'),
        'identities', (SELECT count(*) FROM identities WHERE workspace_id = :'ws'),
        'events', (SELECT count(*) FROM events WHERE workspace_id = :'ws'),
        'events_default_partition', (SELECT count(*) FROM events_default),
        'events_by_name', (SELECT json_object_agg(name, n) FROM
            (SELECT name, count(*) AS n FROM events WHERE workspace_id = :'ws' GROUP BY name) c)),
    'bytes', json_build_object(
        'people_table', pg_table_size('people'),
        'people_indexes', pg_indexes_size('people'),
        'identities_table', pg_table_size('identities'),
        'identities_indexes', pg_indexes_size('identities'),
        'events_table', (SELECT sum(pg_table_size(c.oid)) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid WHERE i.inhparent = 'events'::regclass),
        'events_indexes', (SELECT sum(pg_indexes_size(c.oid)) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid WHERE i.inhparent = 'events'::regclass),
        'people_gin_index', pg_relation_size('people_traits_idx'))
);
SQL
jq . "$M4_OUT/seed.json"
