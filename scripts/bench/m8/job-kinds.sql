-- One snapshot for sample-job-kinds.sh. Ready means due, excluding future timers.
-- jobs has no claim/finish timestamps, and completion deletes the row. Claims, completions
-- and claim-time lag therefore come from worker metrics, not table update/delete estimates.
WITH kinds AS (
  SELECT kind,
         count(*) FILTER (WHERE state = 'ready') AS due_ready,
         count(*) FILTER (WHERE state = 'running') AS running,
         avg(extract(epoch FROM now() - run_at)) FILTER (WHERE state = 'ready') AS ready_lag_mean_seconds,
         max(extract(epoch FROM now() - run_at)) FILTER (WHERE state = 'ready') AS ready_lag_max_seconds
  FROM jobs
  WHERE state = 'running' OR (state = 'ready' AND run_at <= now())
  GROUP BY kind
)
SELECT json_build_object(
  't', to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'kinds', coalesce((SELECT json_agg(kinds ORDER BY kind) FROM kinds), '[]'::json)
);
