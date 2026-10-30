-- #207 cause analysis only: after timers-rearm.sql (worker stopped), move every tenth timer into one
-- burst minute at 300 s of the window, so 90 % stay spread and 10 % wake together in the middle.
BEGIN;
CREATE TEMP TABLE burst ON COMMIT DROP AS
SELECT r.enrollment_id, r.job_id,
       run.t0 + (300 + 60 * (row_number() OVER (ORDER BY r.set, r.n) - 1)::double precision / count(*) OVER ()) * interval '1 second' AS wake_at
FROM m5bench.rows r JOIN m5bench.runs run ON run.run = r.run
WHERE r.run = 1 AND r.n % 10 = 0;
UPDATE m5bench.rows r SET wake_at = b.wake_at FROM burst b WHERE r.run = 1 AND r.enrollment_id = b.enrollment_id;
UPDATE enrollments e SET wake_at = b.wake_at FROM burst b WHERE e.id = b.enrollment_id;
UPDATE jobs j SET run_at = b.wake_at FROM burst b WHERE j.id = b.job_id;
UPDATE m5bench.runs SET burst_at_s = 300, n_burst = (SELECT count(*) FROM burst),
  n_spread = n_spread - (SELECT count(*) FROM burst) WHERE run = 1;
COMMIT;
