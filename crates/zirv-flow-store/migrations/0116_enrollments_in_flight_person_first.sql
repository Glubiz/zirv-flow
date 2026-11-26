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
-- stale statistics they still scan an index in full, as before. The exit walk (`exit_batch`, a flow
-- archive or a version exit) moves too, under stale statistics: a custom plan reads 583 buffers a
-- batch on PostgreSQL 16 against 507 before; on 18 a mid-walk batch reads 3,327 against 507, because
-- it picks `enrollments_occurrence_idx` and walks the flow's completed history as well (first batch
-- 589 to 591 against 506 on both). That is at most one pass over the flow's enrollments, in a rare
-- admin operation, and it takes no new index. Separately, and before this migration, a generic plan
-- reads 12,276 buffers a batch against 10,550, because the cursor cannot be an index condition in it.
--
-- The old index keeps its name, so the swap is one drop and one rename: the new index is built
-- under a temporary name and takes the old name when the old one is dropped.
--
-- One index is added: `enrollments_held_idx (flow_id, person_id) WHERE state = 'held'`. The resume
-- walk (`resume_batch`: `flow_id = $2 AND state = 'held' AND person_id > cursor ORDER BY person_id,
-- id LIMIT 500`) was served by the flow-first in-flight index, which walks the held rows of a flow in
-- person order. With the person-first index the planner, under statistics taken with nothing held,
-- picks `enrollments_flow_state_list_idx` and sorts every held row of the flow on each batch: 9,228
-- buffers a batch against 1,067 (PostgreSQL 16.16 and 18.6, 5,000 held, 20,000 in flight). With the
-- held index it reads 502. The index holds only held rows (pause and resume write it, nothing else
-- does) and serves only statements whose WHERE implies `state = 'held'`.
--
-- UPGRADE COST: both builds (the in-flight index and the held index, each one scan of the table) take
-- SHARE on `enrollments` until the migration commits, which blocks every enrollment insert and
-- update (every flow step) for the builds; reads go on. `CREATE INDEX CONCURRENTLY` cannot run in a migration, because sqlx runs each file in
-- a transaction. To keep the stop short on a large installation, build the new index before it,
-- under the temporary name (the statement below skips an index that exists):
--     CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS enrollments_in_flight_person_idx
--         ON enrollments (person_id, flow_id)
--         WHERE state IN ('active', 'waiting', 'held', 'failed');
-- The migration keeps an index of that name only when it is valid and is this index; one a failed
-- or cancelled build left invalid, or a different index under the name, is dropped and built again
-- here, so a failed pre-build never replaces the old index with an unusable one. The migration
-- then only drops the old index and renames the new one. Those are the last two statements, so the
-- ACCESS EXCLUSIVE lock of the drop, which blocks reads too, is held only until the commit that
-- follows the rename: an instant. The held index is built the same way, under its own name (it needs no
-- swap), so its pre-build is the whole of its cost on a large installation:
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS enrollments_held_idx
--         ON enrollments (flow_id, person_id) WHERE state = 'held';
-- The migration keeps an index of that name only when it is valid and is this index, as above.
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
DO $$
BEGIN
    IF to_regclass('enrollments_held_idx') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_index
        WHERE indexrelid = to_regclass('enrollments_held_idx')
          AND indisvalid AND NOT indisunique AND indrelid = 'enrollments'::regclass
          AND split_part(pg_get_indexdef(indexrelid), ' USING ', 2)
              = 'btree (flow_id, person_id) WHERE (state = ''held''::text)'
    ) THEN
        DROP INDEX enrollments_held_idx;
    END IF;
END
$$;
CREATE INDEX IF NOT EXISTS enrollments_held_idx
    ON enrollments (flow_id, person_id) WHERE state = 'held';
DROP INDEX enrollments_in_flight_idx;
ALTER INDEX enrollments_in_flight_person_idx RENAME TO enrollments_in_flight_idx;
