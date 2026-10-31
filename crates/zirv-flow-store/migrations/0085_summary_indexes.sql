-- The workspace summary (Home figures) counts four things over a window of at most 30 days:
-- events received, people created, messages sent and goal step rows. Each count names the
-- workspace and a time column, and none of them had an index that leads with both, so each
-- would walk the workspace's whole history (or, for messages, every workspace's) on a request.
--
-- Locking: every statement takes a SHARE lock on its table until the index is built, blocking
-- writes. `events` is partitioned, so CREATE INDEX CONCURRENTLY is not possible on the parent;
-- a populated deployment builds the per-partition indexes first with the same name and
-- definition (see 0069), and the same CONCURRENTLY form works for the other three tables:
--     CREATE INDEX CONCURRENTLY events_y2026m10_workspace_id_received_at_idx
--         ON events_y2026m10 (workspace_id, received_at);
-- The events index is on `received_at`, not `occurred_at`: a backfilled or late event has an
-- old `occurred_at` but was received in the window.
CREATE INDEX events_workspace_id_received_at_idx ON events (workspace_id, received_at);

-- New people in a window.
CREATE INDEX people_workspace_created_idx ON people (workspace_id, created_at);

-- Messages sent in a window: only delivered, non-test rows, so the index stays small next to the
-- skipped and failed ones.
CREATE INDEX messages_sent_completed_idx ON messages (workspace_id, completed_at)
    WHERE status = 'sent' AND NOT test;

-- Goal conversions in a window: the step rows started in it. `step_executions_workspace_idx`
-- leads with the node and has no time column.
CREATE INDEX step_executions_workspace_started_idx ON step_executions (workspace_id, started_at);
