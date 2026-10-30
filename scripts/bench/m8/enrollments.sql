-- One resumable M5 staging + loading batch. :slot, :lo, :hi; progress is atomic with all rows.
BEGIN;
SELECT pg_advisory_xact_lock(89393, 0);
SELECT m8bench.ws(:slot)::text AS set \gset
\set run 1
INSERT INTO m8bench.progress VALUES ('enrollments', :'set'::uuid, 0) ON CONFLICT DO NOTHING;
SELECT last_n < :hi AS needed, last_n = :lo - 1 AS contiguous
FROM m8bench.progress WHERE kind = 'enrollments' AND workspace_id = :'set'::uuid FOR UPDATE \gset
\if :needed
  SELECT wake_at > clock_timestamp() + interval '1 hour' AS future_window FROM m8bench.config \gset
  \if :future_window
  \else
    \echo 'M8 waiting window is too near or expired; #396 must re-arm it before continuing'
    SELECT 1/0;
  \endif
  \if :contiguous
  \else
    SELECT 1/0; -- refuse a gap instead of silently skipping rows
  \endif
  INSERT INTO m5bench.rows
  SELECT 1, :'set', g, md5(:'set' || '/p/' || g)::uuid,
         m5bench.uuid7(seed_at), m5bench.uuid7(seed_at), gen_random_uuid(),
         m5bench.uuid7(seed_at), gen_random_uuid(), gen_random_uuid(),
         seed_at, seed_at + interval '13 milliseconds',
         wake_at + ((g - 1)::double precision / m8bench.size((sizes->>'enrollments')::bigint, :slot)) * interval '40 minutes'
  FROM m8bench.config CROSS JOIN generate_series(:lo, :hi) g;
  \set seed_outer_transaction true
  \ir ../m5/seed-load.sql
  UPDATE m8bench.progress SET last_n = :hi WHERE kind = 'enrollments' AND workspace_id = :'set'::uuid;
\endif
COMMIT;
