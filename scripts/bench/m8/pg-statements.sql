-- The 30 statements with the most execution time since the arm's pg_stat_statements_reset() (#397).
-- Query texts are normalised by the extension, so they hold no literal values.
SELECT json_build_object('statements', coalesce(json_agg(s), '[]'::json))
FROM (
  SELECT queryid::text AS queryid, left(regexp_replace(query, '\s+', ' ', 'g'), 300) AS query, calls,
         round(total_exec_time::numeric, 3) AS total_exec_time_ms, round(mean_exec_time::numeric, 3) AS mean_exec_time_ms,
         rows, shared_blks_hit, shared_blks_read, wal_bytes
  FROM pg_stat_statements
  WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
  ORDER BY total_exec_time DESC LIMIT 30
) s;
