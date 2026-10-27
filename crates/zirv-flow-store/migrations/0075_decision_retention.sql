-- Decision-log retention (architecture 16.3, OQ-28). The job drops whole monthly partitions past
-- the window and deletes the rest in batches by `(recorded_at, id)`, the primary key, so it needs
-- no index of its own.

-- The daily purge, as an instance-level schedule: 03:53 UTC, first at the next one.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled)
SELECT gen_random_uuid(), NULL, 'decisions.retention', '53 3 * * *', 'UTC',
       CASE
           WHEN date_trunc('day', now() AT TIME ZONE 'UTC') + interval '3 hours 53 minutes'
                > now() AT TIME ZONE 'UTC'
               THEN (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '3 hours 53 minutes')
                    AT TIME ZONE 'UTC'
           ELSE (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '1 day 3 hours 53 minutes')
                AT TIME ZONE 'UTC'
       END,
       '{}'::jsonb, true
WHERE NOT EXISTS (
    SELECT 1 FROM schedules WHERE kind = 'decisions.retention' AND workspace_id IS NULL
);
