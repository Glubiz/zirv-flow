-- Message retention (architecture 16.3). The purge reads terminal messages oldest first by
-- `(created_at, id)`. `messages_open_idx` (0048) is partial and the other indexes lead with
-- `workspace_id`, so the purge gets its own.
CREATE INDEX messages_created_idx ON messages (created_at, id);

-- The daily purge, as an instance-level schedule: 03:41 UTC, first at the next one.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled)
SELECT gen_random_uuid(), NULL, 'messages.retention', '41 3 * * *', 'UTC',
       CASE
           WHEN date_trunc('day', now() AT TIME ZONE 'UTC') + interval '3 hours 41 minutes'
                > now() AT TIME ZONE 'UTC'
               THEN (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '3 hours 41 minutes')
                    AT TIME ZONE 'UTC'
           ELSE (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '1 day 3 hours 41 minutes')
                AT TIME ZONE 'UTC'
       END,
       '{}'::jsonb, true
WHERE NOT EXISTS (
    SELECT 1 FROM schedules WHERE kind = 'messages.retention' AND workspace_id IS NULL
);
