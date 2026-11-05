-- Per-second I/O counters of M8_IO_DIAG (#397), one JSON line per second via \watch. PostgreSQL 16:
-- pg_stat_wal still holds the WAL write and sync counters; pg_stat_io counts in op_bytes units.
-- Times need track_io_timing and track_wal_io_timing (both sides of the settings A/B set them).
SELECT json_build_object('t', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'wal', (SELECT json_build_object('wal_write', wal_write, 'wal_sync', wal_sync, 'wal_write_time', wal_write_time,
            'wal_sync_time', wal_sync_time, 'wal_buffers_full', wal_buffers_full) FROM pg_stat_wal),
  'io', (SELECT coalesce(json_agg(json_build_object('backend_type', backend_type, 'context', context, 'object', object,
            'writes', writes, 'writebacks', writebacks, 'fsyncs', fsyncs, 'op_bytes', op_bytes)), '[]'::json)
         FROM pg_stat_io WHERE coalesce(writes, 0) > 0 OR coalesce(writebacks, 0) > 0 OR coalesce(fsyncs, 0) > 0),
  'bgwriter', (SELECT json_build_object('maxwritten_clean', maxwritten_clean, 'buffers_clean', buffers_clean) FROM pg_stat_bgwriter),
  'temp_bytes', (SELECT temp_bytes FROM pg_stat_database WHERE datname = current_database()));
\watch 1
