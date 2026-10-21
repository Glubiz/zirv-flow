-- A person erasure marks the running jobs that name the erased people. A running job cannot be
-- deleted by the erasure: its handler holds the row FOR KEY SHARE, and a DELETE conflicts with
-- that while the erasure already holds the people FOR UPDATE. Setting this plain column takes
-- only FOR NO KEY UPDATE, which does not, so the erasure never waits on a lease. Every path that
-- would put the job back to `ready` or `dead` (`fail_job`, `reap_expired`) deletes it instead.
-- The column is in no index, so the update stays a non-key update.
ALTER TABLE jobs ADD COLUMN purge_on_release boolean NOT NULL DEFAULT false;
