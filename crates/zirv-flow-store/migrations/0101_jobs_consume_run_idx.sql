-- The orphaned-run reaper (`fail_orphaned_runs`, every minute from the scheduler leader) looked
-- for a consumer run's job with `unique_key LIKE 'kafka:consume:<run>:%'` and `state <> 'dead'`,
-- neither of which an index served, so it read the whole `jobs` table once per active run (1.5M
-- jobs, 21 active runs: 8.8 s and 31.5M rows filtered per tick).
--
-- `jobs_consume_run_idx` finds the live consumer job of a run by the run id its payload carries
-- (every slice, successors included, has it). It holds only the consumer jobs that are ready or
-- running, about one per active consumer run, so no other enqueue or claim pays for it. The key
-- is not used: a prefix range on `unique_key` is not the same as "starts with" under a non-C
-- database collation, and would miss a live job.
--
-- The build takes a SHARE lock on `jobs` until it is done, which blocks enqueues and claims. A
-- populated deployment builds it first, by hand, with the statement below; the migration then
-- finds it and does nothing. A failed concurrent build leaves an invalid index of its name:
-- drop it and build it again before upgrading.
--
--     CREATE INDEX CONCURRENTLY jobs_consume_run_idx ON jobs ((payload ->> 'run_id'))
--         WHERE kind = 'kafka.consume' AND state IN ('ready', 'running');
CREATE INDEX IF NOT EXISTS jobs_consume_run_idx ON jobs ((payload ->> 'run_id'))
    WHERE kind = 'kafka.consume' AND state IN ('ready', 'running');
