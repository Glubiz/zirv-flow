-- The grouped event set of a segment aggregate (architecture 8.2) reads the person of every event
-- of one name in a window: `SELECT e.person_id FROM events e WHERE e.workspace_id = $1 AND
-- e.name = $2 AND e.occurred_at > $3 ... GROUP BY e.person_id`. `events_name_idx` (workspace_id,
-- name, occurred_at DESC) finds those rows but not their person, so each one was read from the
-- table, and a parallel plan read the whole window once per participant (#397). With `person_id`
-- in the index the set is an index-only scan. The new index replaces the old one, so an insert
-- writes as many index entries as before, and every reader of the old index (the explorer by
-- name, the retention by event schema) reads the same leading columns.
--
-- Locking: dropping `events_name_idx` takes ACCESS EXCLUSIVE on `events` and every partition, and
-- a partitioned index cannot be dropped CONCURRENTLY. The drop goes first, waits at most 2 s for
-- that lock per attempt, ten attempts one second apart, and then fails with SQLSTATE 55P03, so a
-- failure costs nothing: the file rolls back whole. Building a missing partition index takes
-- SHARE on its partition. A populated deployment builds the partition indexes CONCURRENTLY before
-- the stop, creates the parent index ON ONLY, attaches each partition index and drops the old
-- index (docs/operations/upgrade.md, "0091"); this file then only checks that work. It also
-- resumes a manual run that stopped halfway: a partition without the index gets it, an invalid
-- index a failed CONCURRENTLY build left is built again, and the parent is valid at the end.

DO $$
DECLARE
    previous text := current_setting('lock_timeout');
    attempt integer := 0;
BEGIN
    PERFORM set_config('lock_timeout', '2s', true);
    LOOP
        attempt := attempt + 1;
        BEGIN
            DROP INDEX IF EXISTS events_name_idx;
            EXIT;
        EXCEPTION WHEN lock_not_available THEN
            IF attempt >= 10 THEN
                RAISE EXCEPTION 'events_name_idx was not dropped: another session held a lock on events through % attempts', attempt
                    USING ERRCODE = 'lock_not_available',
                          HINT = 'Find the session in pg_locks, end it deliberately and migrate again (docs/operations/upgrade.md, 0091).';
            END IF;
            PERFORM pg_sleep(1);
        END;
    END LOOP;
    PERFORM set_config('lock_timeout', previous, true);
END
$$;

CREATE INDEX IF NOT EXISTS events_name_person_idx
    ON ONLY events (workspace_id, name, occurred_at DESC) INCLUDE (person_id);

-- Each partition index has the name PostgreSQL gives the one it creates for a new partition.
DO $$
DECLARE
    part record;
    child regclass;
    index_name text;
BEGIN
    FOR part IN
        SELECT c.oid, c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent = 'events'::regclass ORDER BY c.relname
    LOOP
        SELECT x.indexrelid::regclass INTO child
        FROM pg_inherits a JOIN pg_index x ON x.indexrelid = a.inhrelid
        WHERE a.inhparent = 'events_name_person_idx'::regclass AND x.indrelid = part.oid;
        IF child IS NOT NULL AND (SELECT indisvalid FROM pg_index WHERE indexrelid = child) THEN
            CONTINUE;
        ELSIF child IS NOT NULL THEN
            -- An attached index cannot be dropped, so an invalid one is rebuilt in place.
            EXECUTE format('REINDEX INDEX %s', child);
        ELSE
            index_name := part.relname || '_workspace_id_name_occurred_at_person_id_idx';
            IF EXISTS (
                SELECT 1 FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
                WHERE x.indrelid = part.oid AND ic.relname = index_name AND NOT x.indisvalid
            ) THEN
                EXECUTE format('DROP INDEX %I', index_name);
            END IF;
            EXECUTE format(
                'CREATE INDEX IF NOT EXISTS %I ON %I (workspace_id, name, occurred_at DESC) '
                'INCLUDE (person_id)', index_name, part.relname);
            child := index_name::regclass;
        END IF;
        -- Attaching validates the parent once every partition has a valid index attached.
        EXECUTE format('ALTER INDEX events_name_person_idx ATTACH PARTITION %s', child);
    END LOOP;
    -- Attaching an attached index only validates the parent again, which a parent left invalid
    -- after its last index was rebuilt by hand needs.
    IF NOT (SELECT indisvalid FROM pg_index WHERE indexrelid = 'events_name_person_idx'::regclass) THEN
        SELECT a.inhrelid::regclass INTO child FROM pg_inherits a
        WHERE a.inhparent = 'events_name_person_idx'::regclass LIMIT 1;
        EXECUTE format('ALTER INDEX events_name_person_idx ATTACH PARTITION %s', child);
    END IF;
    IF NOT (SELECT indisvalid FROM pg_index WHERE indexrelid = 'events_name_person_idx'::regclass) THEN
        RAISE EXCEPTION 'events_name_person_idx is not valid after every partition was attached';
    END IF;
END
$$;
