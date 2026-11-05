-- The settle of a wait step (`settle_wait_step`, `waiting` to `done`) can be a HOT update once no
-- index holds `outcome` (#397): when its page has room, the row is rewritten on that page and no
-- index gets an entry. A settle on a full page is not HOT (the table keeps the default
-- fillfactor); a bench-shaped run measured 59% of settles HOT. Two indexes held it:
--
-- - `step_executions_workspace_idx` (workspace_id, node_id, outcome) has no reader. The per-node
--   counts of a flow join the step rows to the enrollments of the version through the primary key
--   since 0040, and every other read finds step rows by enrollment. A read by workspace would use
--   the started index below, which leads with the same two columns.
-- - `step_executions_workspace_node_started_idx` carried `outcome` for the rollup's goal count. It
--   is rebuilt without it, as `step_executions_started_idx`; the count reads the outcome of the
--   few goal rows of its slice from the table.
--
-- The enrollments of a deleted person are found by `person_id` alone (#510). The cascade from
-- `people` deletes `WHERE person_id = $1`, which `enrollments_person_idx` (workspace_id,
-- person_id) cannot seek, so each deleted person read a whole index or the table. A person
-- belongs to one workspace, so the reads by workspace and person find the same rows through
-- `enrollments_person_id_idx`. One index replaces another, so a wake writes as many index entries
-- as before. The cascade of a workspace's own delete now reads the table once.
--
-- Locking: each build takes a SHARE lock on its table until it is done, and each drop an ACCESS
-- EXCLUSIVE lock; both block every flow step, and on `enrollments` every enrollment too. A
-- populated deployment runs these first, by hand and outside a transaction, and the statements
-- below then find nothing to do:
--
--     CREATE INDEX CONCURRENTLY step_executions_started_idx
--         ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id);
--     DROP INDEX CONCURRENTLY IF EXISTS step_executions_workspace_node_started_idx;
--     DROP INDEX CONCURRENTLY IF EXISTS step_executions_workspace_idx;
--     CREATE INDEX CONCURRENTLY enrollments_person_id_idx ON enrollments (person_id);
--     DROP INDEX CONCURRENTLY IF EXISTS enrollments_person_idx;
--
-- A failed CONCURRENTLY build leaves an invalid index of that name: drop it and build again.
CREATE INDEX IF NOT EXISTS step_executions_started_idx
    ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id);
DROP INDEX IF EXISTS step_executions_workspace_node_started_idx;
DROP INDEX IF EXISTS step_executions_workspace_idx;
CREATE INDEX IF NOT EXISTS enrollments_person_id_idx ON enrollments (person_id);
DROP INDEX IF EXISTS enrollments_person_idx;
