-- Only the M8 fixture enrollments are rewound; the worker is stopped by timers.sh.
-- Reuse the M5 product-path templates and lateness query, including the original wake job ids.
BEGIN;
SELECT pg_advisory_xact_lock(89393, 0);
SELECT current_database() = 'zirv_flow_m8'
  AND (SELECT count(*) FROM m5bench.rows WHERE run = 1) = :enrollments
  AND (SELECT sum(last_n) FROM m8bench.progress WHERE kind = 'enrollments') = :enrollments
  AND (SELECT count(*) FROM enrollments e JOIN m5bench.rows r ON r.enrollment_id = e.id WHERE r.run = 1) = :enrollments
  AND NOT EXISTS (SELECT 1 FROM jobs j JOIN m5bench.rows r ON r.job_id = j.id WHERE r.run = 1 AND j.state = 'running') AS ready
\gset
\if :ready
\else
  DO $$ BEGIN RAISE EXCEPTION 'complete M8 enrollment seed and stopped worker required'; END $$;
\endif

DELETE FROM step_executions s USING m5bench.rows r
WHERE r.run = 1 AND s.enrollment_id = r.enrollment_id AND s.node_id = 'x1';
UPDATE step_executions s SET outcome = t.row->>'outcome', error = NULL,
  attempts = (t.row->>'attempts')::integer, started_at = r.step_at
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'step'
WHERE r.run = 1 AND s.id = r.step_id;
DELETE FROM jobs j USING m5bench.rows r WHERE r.run = 1 AND j.id = r.job_id;
INSERT INTO m5bench.runs (run, t0, window_s, burst_at_s, n_spread, n_burst)
VALUES (1, clock_timestamp() + :lead * interval '1 second', :seconds, 0, :enrollments, 0)
ON CONFLICT (run) DO UPDATE SET t0 = excluded.t0, window_s = excluded.window_s,
  burst_at_s = 0, n_spread = excluded.n_spread, n_burst = 0;

UPDATE m5bench.rows r SET wake_at = run.t0
  + (r.n - 1)::double precision / m8bench.size(:enrollments, g) * :seconds * interval '1 second'
FROM m5bench.runs run, generate_series(1, 3) g
WHERE r.run = 1 AND run.run = 1 AND r.set = m8bench.ws(g)::text;
UPDATE enrollments e SET state = t.row->>'state', node = t.row->>'node',
  state_version = (t.row->>'state_version')::bigint, wake_at = r.wake_at,
  ended_at = NULL, exit_reason = NULL, error = NULL, failed_node = NULL,
  resume_input = NULL, failure_placed = false, updated_at = clock_timestamp()
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'enrollment'
WHERE r.run = 1 AND e.id = r.enrollment_id;
INSERT INTO jobs
SELECT (jsonb_populate_record(NULL::jobs, t.row || jsonb_build_object(
  'id', r.job_id, 'run_at', r.wake_at, 'created_at', clock_timestamp(), 'trace_id', r.trace_id,
  'payload', (t.row->'payload') || jsonb_build_object('enrollment_id', r.enrollment_id),
  'unique_key', 'wake:' || r.enrollment_id || ':' || (t.row->'payload'->>'state_version')))).*
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'job'
WHERE r.run = 1;

SELECT t0 > clock_timestamp() + interval '5 seconds' AS future_window FROM m5bench.runs WHERE run = 1
\gset
\if :future_window
\else
  DO $$ BEGIN RAISE EXCEPTION 're-arm exceeded lead time; increase M8_TIMERS_LEAD_SECONDS'; END $$;
\endif
COMMIT;
