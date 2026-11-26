-- Two hot statements looked up the in-flight enrollments of a flow by person: the re-entry check
-- (`flow_id = $1 AND person_id = ANY($2)`) and the goal and exit lookup of every drain batch
-- (`person_id = t.person_id` over the people of the arrivals). `enrollments_in_flight_idx` was
-- `(flow_id, person_id)`, and its predicate holds only the in-flight rows. When autoanalyze samples
-- `enrollments` while nothing is in flight (the gap between two bench steps), `state` is recorded as
-- 100 % ended and the planner believes the partial index is empty, so a scan of all of it costs less
-- than any probe. Both statements then read every in-flight enrollment on each call: with 20,000 in
-- flight, about 20,100 buffer hits and 9 to 110 ms a call (PostgreSQL 16.16 and 18.6), against about
-- 121 hits and 0.1 ms with fresh statistics. The cost grows with the in-flight count through a step,
-- which is the stall the tier L bench saw until the next autoanalyze.
--
-- The index is rebuilt person first, `(person_id, flow_id)`, with the same predicate, the same
-- uniqueness and the same name. A probe for one person then finds its few entries by an equality on
-- the leading column, and the two statements run one probe per person (see `in_flight_of` and
-- `enqueue_event_settles`), which the planner cannot turn into a scan of the whole set. The
-- `ON CONFLICT (flow_id, person_id)` arbiters infer the index whatever its column order. What moves:
-- the flow-scoped in-flight counts (`count_in_flight_enrollments`, the `summary.rollup` flow
-- snapshot) no longer have `flow_id` leading in this index, so they are answered by
-- `enrollments_flow_state_idx (flow_id, state, flow_version)` when the statistics are fresh. Under
-- stale statistics they still scan an index in full, as before.
--
-- The old index keeps its name, so the swap is one drop and one rename: the new index is built
-- under a temporary name and takes the old name when the old one is dropped. The table has no more
-- indexes than before.
--
-- UPGRADE COST: the build takes SHARE on `enrollments` until the migration commits, which blocks
-- every enrollment insert and update (every flow step) for the build, which scans the table once;
-- reads go on. `CREATE INDEX CONCURRENTLY` cannot run in a migration, because sqlx runs each file in
-- a transaction. To keep the stop short on a large installation, build the new index before it,
-- under the temporary name (the statement below skips an index that exists):
--     CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS enrollments_in_flight_person_idx
--         ON enrollments (person_id, flow_id)
--         WHERE state IN ('active', 'waiting', 'held', 'failed');
-- The migration keeps an index of that name only when it is valid and is this index; one a failed
-- or cancelled build left invalid, or a different index under the name, is dropped and built again
-- here, so a failed pre-build never replaces the old index with an unusable one. The migration
-- then only drops the old index and renames the new one, which takes ACCESS EXCLUSIVE on the
-- table for an instant.
DO $$
BEGIN
    IF to_regclass('enrollments_in_flight_person_idx') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_index
        WHERE indexrelid = to_regclass('enrollments_in_flight_person_idx')
          AND indisvalid AND indisunique AND indrelid = 'enrollments'::regclass
          AND split_part(pg_get_indexdef(indexrelid), ' USING ', 2)
              = 'btree (person_id, flow_id) WHERE (state = ANY (ARRAY[''active''::text, ''waiting''::text, ''held''::text, ''failed''::text]))'
    ) THEN
        DROP INDEX enrollments_in_flight_person_idx;
    END IF;
END
$$;
CREATE UNIQUE INDEX IF NOT EXISTS enrollments_in_flight_person_idx
    ON enrollments (person_id, flow_id)
    WHERE state IN ('active', 'waiting', 'held', 'failed');
DROP INDEX enrollments_in_flight_idx;
ALTER INDEX enrollments_in_flight_person_idx RENAME TO enrollments_in_flight_idx;
