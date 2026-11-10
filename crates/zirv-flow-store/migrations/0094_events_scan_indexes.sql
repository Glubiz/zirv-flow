-- Two scans of `events` read the heap for columns an index could carry.
--
--  * The full segment refresh counts events per person by name over a window. It walked
--    `events_name_idx` (workspace_id, name, occurred_at DESC) and fetched the heap row of every
--    entry only for `person_id`: on the sim 469k of 710k block reads and 43.6 of 48.5 s of one
--    refresh. `INCLUDE (person_id)` makes that scan index-only.
--  * The re-evaluation diff reads a batch of people's events through `events_person_idx`
--    (workspace_id, person_id, occurred_at DESC) and filtered on `name` at the heap: 332 rows
--    kept, 13,772 removed by the filter, 94% of its time in reads. Appending `name` as the last
--    key column turns the filter into an index condition, and the heap is visited only for the
--    matching events. It adds no index to the insert path.
--
-- The index names are kept. Both are created on the partitioned parent, so every partition,
-- existing and future, gets its own.
--
-- UPGRADE COST: this rebuilds both indexes on every events partition. CREATE INDEX
-- CONCURRENTLY is not allowed on a partitioned table, and sqlx runs a migration file as one
-- statement batch, so each build takes a SHARE lock on its partition and blocks writes to it
-- (every event ingest into that month) until the migration commits, for as long as the build of
-- all partitions takes. It grows with the stored events; the indexes grow by about 29% (name)
-- and 34% (person). Plan the upgrade for a quiet window.
DROP INDEX events_name_idx;
DROP INDEX events_person_idx;
CREATE INDEX events_name_idx ON events (workspace_id, name, occurred_at DESC) INCLUDE (person_id);
CREATE INDEX events_person_idx ON events (workspace_id, person_id, occurred_at DESC, name);

-- An index-only scan only skips the heap for pages marked all-visible. Events are insert-only, so
-- autovacuum reaches a partition only after the default 20% growth of its rows (the insert scale
-- factor), and the current month's visibility map lags far behind. Each partition instead gets a
-- fixed insert threshold: vacuumed every 500,000 inserted rows, which sets the visibility map.
-- A partitioned parent cannot hold autovacuum options, so they are set on every partition here
-- (the default one included) and by `ensure_event_partitions` on the ones it creates.
-- `ALTER TABLE ... SET (storage parameters)` takes SHARE UPDATE EXCLUSIVE, which blocks neither
-- reads nor writes.
DO $$
DECLARE
    child regclass;
BEGIN
    FOR child IN
        SELECT inhrelid::regclass FROM pg_inherits WHERE inhparent = 'events'::regclass
    LOOP
        EXECUTE format(
            'ALTER TABLE %s SET (autovacuum_vacuum_insert_scale_factor = 0, '
            'autovacuum_vacuum_insert_threshold = 500000)',
            child
        );
    END LOOP;
END
$$;
