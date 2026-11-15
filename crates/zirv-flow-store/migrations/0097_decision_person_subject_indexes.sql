-- `delete_person` removed a person's decisions with a predicate over `jsonb_each_text` of every
-- row's `subject_ids`, which no index can serve: one call read every decision record of the
-- workspace in every partition (1.5M rows in 11 partitions: 1,784 ms and 39,476 buffers read to
-- delete 8 rows), so an orphan purge cost orphans x decisions.
--
-- A person id sits only under a closed set of subject names (`PERSON_SUBJECTS` in the store's
-- `privacy::delete`, checked against `decision_actions::ALL` by a test): `person`, which
-- `decision_records_person_idx` (0012) already serves, and the four below. They appear only on
-- identity and merge decisions, so each index is small and a partial index adds nothing to the
-- insert of any other decision.
--
-- The indexes are created on the partitioned parent, so every partition, existing and future,
-- gets its own.
--
-- UPGRADE COST: CREATE INDEX CONCURRENTLY is not allowed on a partitioned table, and the build
-- takes SHARE on `decision_records` and on every partition, held to the commit. Decision writes
-- (every ingest, flow step and send) wait until the four indexes are built on all partitions,
-- and the wait grows with the stored decisions (reads go on). Plan the upgrade for a quiet window.
CREATE INDEX decision_records_other_person_idx
    ON decision_records (workspace_id, (subject_ids ->> 'other_person'))
    WHERE subject_ids ? 'other_person';
CREATE INDEX decision_records_previous_person_idx
    ON decision_records (workspace_id, (subject_ids ->> 'previous_person'))
    WHERE subject_ids ? 'previous_person';
CREATE INDEX decision_records_survivor_idx
    ON decision_records (workspace_id, (subject_ids ->> 'survivor'))
    WHERE subject_ids ? 'survivor';
CREATE INDEX decision_records_merged_idx
    ON decision_records (workspace_id, (subject_ids ->> 'merged'))
    WHERE subject_ids ? 'merged';
