-- Wait events of the client backends, once per second via \watch (#397). Ingest backends are those
-- whose current or last statement touches the inbox; the rest are listed as other.
-- Variables (the I/O diagnostics set both): interval, the seconds between samples (default 1), and keyed,
-- which samples every non-idle backend keyed by application_name and backend_type instead.
\if :{?interval}
\else
\set interval 1
\endif
\if :{?keyed}
SELECT json_build_object('t', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'waits', coalesce(json_agg(w), '[]'::json))
FROM (
  SELECT application_name, backend_type, state, wait_event_type, wait_event, count(*) AS backends
  FROM pg_stat_activity
  WHERE pid <> pg_backend_pid() AND state IS DISTINCT FROM 'idle' AND application_name NOT LIKE 'm8-sampler%'
    AND (datname = current_database() OR datname IS NULL)
  GROUP BY 1, 2, 3, 4, 5
) w
\else
SELECT json_build_object('t', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'waits', coalesce(json_agg(w), '[]'::json))
FROM (
  SELECT query ILIKE '%ingest_inbox%' AS ingest, state, wait_event_type, wait_event, count(*) AS backends
  FROM pg_stat_activity
  WHERE backend_type = 'client backend' AND datname = current_database() AND pid <> pg_backend_pid()
  GROUP BY 1, 2, 3, 4
) w
\endif
\watch :interval
