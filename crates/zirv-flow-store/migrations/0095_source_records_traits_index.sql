-- The ledger reads that need only the records holding traits: a batch's trait recompute
-- (`people/batch.rs`, the key lookup), a merge's read of the survivor's keys (`people/merge.rs`)
-- and `traits.recompute` (`people/recompute.rs`). Most records hold no traits (a Kafka message's
-- record carries identifiers only), yet each read walked `source_records_person_idx` and fetched
-- every live record of its people from the heap. This index lists only the live records that
-- hold traits; each read adds `traits <> '{}'`, which changes none of its rows (a record without
-- traits holds no key), so the planner can prove the index's predicate.
--
-- `source_records_person_idx` stays: the merge's move of a person's records, the batch lock of
-- a merging owner's records, the orphan check, the identity release, the profile and the
-- privacy export and delete read every record of a person, with or without traits.
--
-- Write cost: an insert or a non-HOT update adds an entry here only when the new row is live and
-- holds traits (none for a Kafka message's record). `traits` now takes part in the HOT check, so
-- an update that changes a record's traits is never heap-only; an update that leaves `traits`
-- alone (`last_seen_run`) still can be.
--
-- UPGRADE COST: the build takes SHARE on `source_records` until the migration commits, which
-- blocks every write to the ledger (the drain's profile records, sync and Kafka units, imports,
-- contacts) for the build; reads go on. `docs/operations/upgrade.md` gives its measured
-- duration. The file does not build concurrently: it would run under the migration advisory
-- lock, and a second migrator waiting for that lock deadlocks a concurrent build. A populated
-- deployment can build it first, outside a transaction, under the same name:
--     CREATE INDEX CONCURRENTLY source_records_traits_idx ON source_records (workspace_id, person_id)
--         WHERE deleted_at IS NULL AND traits <> '{}'::jsonb;
-- The migration keeps an index of that name only when it is valid and is this index; one a failed
-- build left invalid, or a different index under the name, is dropped and built again here.
DO $$
BEGIN
    IF to_regclass('source_records_traits_idx') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_index
        WHERE indexrelid = to_regclass('source_records_traits_idx')
          AND indisvalid AND NOT indisunique AND indrelid = 'source_records'::regclass
          AND split_part(pg_get_indexdef(indexrelid), ' USING ', 2)
              = 'btree (workspace_id, person_id) WHERE ((deleted_at IS NULL) AND (traits <> ''{}''::jsonb))'
    ) THEN
        DROP INDEX source_records_traits_idx;
    END IF;
END
$$;
CREATE INDEX IF NOT EXISTS source_records_traits_idx ON source_records (workspace_id, person_id)
    WHERE deleted_at IS NULL AND traits <> '{}'::jsonb;
