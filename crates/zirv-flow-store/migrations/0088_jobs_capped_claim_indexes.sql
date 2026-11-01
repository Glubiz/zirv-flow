-- The claim under `worker.max_running_per_workspace` (`jobs.rs`, #207) must not step over the due
-- jobs of a workspace at its cap. `jobs_ready_idx` orders a priority band by `run_at` alone, so a
-- capped workspace's flood of due jobs sits ahead of every other workspace's job in the band and
-- each claim would read all of it.
--
-- `jobs_ready_workspace_idx` orders the ready jobs by workspace, then band, then age. The capped
-- claim walks it one workspace and band at a time (a skip scan): it passes over a capped
-- workspace at once and reads only the oldest due job of each other one. Its leading column is
-- the workspace so that no other index can serve those probes, whatever the statistics say.
-- Instance jobs (no workspace) are never capped; `jobs_ready_instance_idx` holds them alone, so
-- their probe does not step over the workspaces' jobs either. Each ready job is in exactly one
-- of the two.
--
-- Each build takes a SHARE lock on `jobs` until it is done, which blocks enqueues and claims. A
-- populated deployment builds them first, by hand, with the statements below; the migration then
-- finds them and does nothing. A failed concurrent build leaves an invalid index of its name:
-- drop it and build it again before upgrading.
--
--     CREATE INDEX CONCURRENTLY jobs_ready_workspace_idx ON jobs (workspace_id, priority, run_at)
--         WHERE state = 'ready' AND workspace_id IS NOT NULL;
--     CREATE INDEX CONCURRENTLY jobs_ready_instance_idx ON jobs (priority, run_at)
--         WHERE state = 'ready' AND workspace_id IS NULL;
CREATE INDEX IF NOT EXISTS jobs_ready_workspace_idx ON jobs (workspace_id, priority, run_at)
    WHERE state = 'ready' AND workspace_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS jobs_ready_instance_idx ON jobs (priority, run_at)
    WHERE state = 'ready' AND workspace_id IS NULL;
