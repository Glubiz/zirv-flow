-- One JSON snapshot of the WAL, I/O and checkpoint counters, taken before and after each arm (#397).
-- PostgreSQL 17 moved the checkpoint counters from pg_stat_bgwriter to pg_stat_checkpointer.
SELECT to_regclass('pg_stat_checkpointer') IS NOT NULL AS has_checkpointer \gset
\if :has_checkpointer
SELECT json_build_object('t', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'wal_lsn', pg_current_wal_lsn()::text, 'wal_lsn_bytes', pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0'),
  'pg_stat_wal', (SELECT to_json(w) FROM pg_stat_wal w),
  'pg_stat_io', (SELECT json_agg(to_json(i)) FROM pg_stat_io i),
  'checkpoints', (SELECT to_json(c) FROM pg_stat_checkpointer c),
  'bgwriter', (SELECT to_json(b) FROM pg_stat_bgwriter b));
\else
SELECT json_build_object('t', to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
  'wal_lsn', pg_current_wal_lsn()::text, 'wal_lsn_bytes', pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0'),
  'pg_stat_wal', (SELECT to_json(w) FROM pg_stat_wal w),
  'pg_stat_io', (SELECT json_agg(to_json(i)) FROM pg_stat_io i),
  'checkpoints', (SELECT to_json(b) FROM pg_stat_bgwriter b),
  'bgwriter', (SELECT to_json(b) FROM pg_stat_bgwriter b));
\endif
