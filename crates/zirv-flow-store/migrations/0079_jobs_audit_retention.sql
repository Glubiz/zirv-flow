-- Retention of dead job history (14 days) and of the audit log (2 years), architecture 16.3. Both
-- jobs are daily, instance-level schedules at minutes the other retention schedules do not use
-- (03:17, 03:41 and 03:53): 04:07 UTC for dead jobs and 04:19 UTC for the audit log, first at the
-- next one.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled)
SELECT gen_random_uuid(), NULL, v.kind, v.cron, 'UTC',
       CASE
           WHEN date_trunc('day', now() AT TIME ZONE 'UTC') + v.at > now() AT TIME ZONE 'UTC'
               THEN (date_trunc('day', now() AT TIME ZONE 'UTC') + v.at) AT TIME ZONE 'UTC'
           ELSE (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '1 day' + v.at)
                AT TIME ZONE 'UTC'
       END,
       '{}'::jsonb, true
FROM (VALUES
    ('jobs.retention', '7 4 * * *', interval '4 hours 7 minutes'),
    ('audit.retention', '19 4 * * *', interval '4 hours 19 minutes')
) AS v (kind, cron, at)
WHERE NOT EXISTS (
    SELECT 1 FROM schedules WHERE kind = v.kind AND workspace_id IS NULL
);
