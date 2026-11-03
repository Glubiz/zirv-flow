-- Wait events of the client backends, once per second via \watch (#397). Ingest backends are those
-- whose current or last statement touches the inbox; the rest are listed as other.
SELECT json_build_object('t', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'waits', coalesce(json_agg(w), '[]'::json))
FROM (
  SELECT query ILIKE '%ingest_inbox%' AS ingest, state, wait_event_type, wait_event, count(*) AS backends
  FROM pg_stat_activity
  WHERE backend_type = 'client backend' AND datname = current_database() AND pid <> pg_backend_pid()
  GROUP BY 1, 2, 3, 4
) w
\watch 1
