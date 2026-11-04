-- The settle of a wait step (`settle_wait_step`, `waiting` to `done`) becomes a HOT update when no
-- index holds `outcome` (#397): the row is rewritten on its own page and no index gets an entry.
-- Two indexes held it:
--
-- - `step_executions_workspace_idx` (workspace_id, node_id, outcome) has no reader. The per-node
--   counts of a flow join the step rows to the enrollments of the version through the primary key
--   since 0040, and every other read finds step rows by enrollment. A read by workspace would use
--   the started index below, which leads with the same two columns.
-- - `step_executions_workspace_node_started_idx` carried `outcome` for the rollup's goal count. It
--   is rebuilt without it, as `step_executions_started_idx`; the count reads the outcome of the
--   few goal rows of its slice from the table.
--
-- Locking: the build takes a SHARE lock on `step_executions` until it is done, and each drop an
-- ACCESS EXCLUSIVE lock; both block every flow step. A populated deployment runs these first, by
-- hand and outside a transaction, and the statements below then find nothing to do:
--
--     CREATE INDEX CONCURRENTLY step_executions_started_idx
--         ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id);
--     DROP INDEX CONCURRENTLY IF EXISTS step_executions_workspace_node_started_idx;
--     DROP INDEX CONCURRENTLY IF EXISTS step_executions_workspace_idx;
--
-- A failed CONCURRENTLY build leaves an invalid index of that name: drop it and build again.
CREATE INDEX IF NOT EXISTS step_executions_started_idx
    ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id);
DROP INDEX IF EXISTS step_executions_workspace_node_started_idx;
DROP INDEX IF EXISTS step_executions_workspace_idx;
