-- The wake-up check of an enqueue (`jobs.rs`) asks whether a ready, due job of one kind already
-- exists, so that a busy kind sends no NOTIFY. `jobs_ready_idx` is ordered by (priority, run_at)
-- and cannot answer that by kind: with a large due backlog of another kind every enqueue would
-- scan it. This index makes the check a single probe.
CREATE INDEX jobs_ready_kind_idx ON jobs (kind, run_at) WHERE state = 'ready';
