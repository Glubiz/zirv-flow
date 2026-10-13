-- Source freshness (architecture section 7.5): what a stream needs to say whether its data is
-- current. The state itself (fresh, stale, failing) is derived from these columns and is never
-- stored. No person id, no record value.

-- When the stream was last enabled: the grace period of a stream that has never succeeded
-- starts here. Null while the stream is disabled.
ALTER TABLE streams ADD COLUMN enabled_at timestamptz;
-- When the stream last succeeded: a finished pull run, or what a webhook or a consumer reports.
ALTER TABLE streams ADD COLUMN last_success_at timestamptz;
-- Whether the last verdict on the stream was a failure.
ALTER TABLE streams ADD COLUMN last_run_failed boolean NOT NULL DEFAULT false;

-- Streams already enabled start their grace period at the upgrade, so none turns stale at once.
UPDATE streams SET enabled_at = now() WHERE enabled;

UPDATE streams s SET last_success_at = r.finished_at
FROM (SELECT stream_id, max(finished_at) AS finished_at
      FROM sync_runs WHERE state = 'succeeded' GROUP BY stream_id) r
WHERE r.stream_id = s.id;

-- The latest finished run that was not cancelled decides whether the stream is failing.
UPDATE streams s SET last_run_failed = true
FROM (SELECT DISTINCT ON (stream_id) stream_id, state
      FROM sync_runs WHERE state IN ('succeeded', 'failed')
      ORDER BY stream_id, finished_at DESC NULLS LAST, id DESC) r
WHERE r.stream_id = s.id AND r.state = 'failed';

ALTER TABLE streams ADD CONSTRAINT streams_enabled_at_check CHECK (enabled OR enabled_at IS NULL);
