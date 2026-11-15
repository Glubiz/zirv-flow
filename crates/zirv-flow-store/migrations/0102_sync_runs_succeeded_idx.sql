-- `is_first_sync` asks, for every batch that carries an event, whether the stream has an earlier
-- succeeded sync run. No index served the question (`sync_runs_one_active_idx` holds only queued
-- and running runs, `sync_runs_history_idx` is by source and time), so a stream that had not yet
-- succeeded read the workspace's whole run history per batch (200k runs: 22 ms and 5.7k buffers).
--
-- `sync_runs_succeeded_idx` holds one small entry per succeeded sync run, by workspace and
-- stream, so the question is one probe. A running run is never in it. Run history is not pruned.
--
-- The build takes a SHARE lock on `sync_runs` until it is done, which blocks run starts and
-- finishes. A populated deployment builds it first, by hand, with the statement below; the
-- migration then finds it and does nothing. A failed concurrent build leaves an invalid index of
-- its name: drop it and build it again before upgrading.
--
--     CREATE INDEX CONCURRENTLY sync_runs_succeeded_idx ON sync_runs (workspace_id, stream_id)
--         WHERE state = 'succeeded' AND run_kind = 'sync';
CREATE INDEX IF NOT EXISTS sync_runs_succeeded_idx ON sync_runs (workspace_id, stream_id)
    WHERE state = 'succeeded' AND run_kind = 'sync';
