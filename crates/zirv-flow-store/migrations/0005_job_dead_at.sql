-- When a job went dead, for the dead-letter list. The fail and reap paths set it, and a
-- retry clears it. Jobs that are dead before this migration get the migration time.
ALTER TABLE jobs ADD COLUMN dead_at timestamptz;

UPDATE jobs SET dead_at = now() WHERE state = 'dead';

-- Keyset pagination of the dead-letter list, newest first.
CREATE INDEX jobs_dead_idx ON jobs (dead_at DESC, id DESC) WHERE state = 'dead';
