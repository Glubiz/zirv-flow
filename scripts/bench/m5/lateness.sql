-- Timer lateness of a seeded run: the start of the step that follows the wait (node x1, its
-- step_executions.started_at, the transaction start of the flow.step wake job) minus the wake
-- instant the seed chose (m5bench.rows.wake_at; the runtime clears enrollments.wake_at on completion).
-- Percentiles are nearest-rank (percentile_disc), as m4's stats. Prints one JSON document.
-- psql variables: run, x1 (the node after the wait, default x1).
WITH r AS (SELECT * FROM m5bench.rows WHERE run = :run),
run AS (SELECT * FROM m5bench.runs WHERE run = :run),
late AS (
  SELECT r.set, r.n, r.wake_at, s.started_at,
         extract(epoch FROM s.started_at - r.wake_at) AS late_s,
         -- the spread timers that fall in the burst's minute are reported on their own
         (r.set = 'spread' AND r.wake_at >= run.t0 + run.burst_at_s * interval '1 second'
                           AND r.wake_at < run.t0 + (run.burst_at_s + 60) * interval '1 second') AS in_burst_minute,
         floor(extract(epoch FROM r.wake_at - run.t0) / 60)::int AS minute
  FROM r CROSS JOIN run
  LEFT JOIN step_executions s ON s.enrollment_id = r.enrollment_id AND s.node_id = 'x1'
),
agg AS (
  SELECT set, in_burst_minute, count(*) AS seeded, count(late_s) AS woken,
         min(late_s) AS min_s, percentile_disc(0.5) WITHIN GROUP (ORDER BY late_s) AS p50_s,
         percentile_disc(0.95) WITHIN GROUP (ORDER BY late_s) AS p95_s,
         percentile_disc(0.99) WITHIN GROUP (ORDER BY late_s) AS p99_s,
         max(late_s) AS max_s, count(*) FILTER (WHERE late_s < 0) AS early
  FROM late GROUP BY GROUPING SETS ((set), (in_burst_minute), ())
  HAVING NOT (set IS NULL AND in_burst_minute IS FALSE)
),
spread_outside AS (
  SELECT count(*) AS seeded, count(late_s) AS woken, min(late_s) AS min_s,
         percentile_disc(0.5) WITHIN GROUP (ORDER BY late_s) AS p50_s,
         percentile_disc(0.95) WITHIN GROUP (ORDER BY late_s) AS p95_s,
         percentile_disc(0.99) WITHIN GROUP (ORDER BY late_s) AS p99_s,
         max(late_s) AS max_s, count(*) FILTER (WHERE late_s < 0) AS early
  FROM late WHERE set = 'spread' AND NOT in_burst_minute
),
per_minute AS (
  SELECT set, minute, count(*) AS seeded, count(late_s) AS woken,
         percentile_disc(0.5) WITHIN GROUP (ORDER BY late_s) AS p50_s,
         percentile_disc(0.99) WITHIN GROUP (ORDER BY late_s) AS p99_s, max(late_s) AS max_s
  FROM late GROUP BY set, minute
)
SELECT json_build_object(
  'run', :run,
  't0', (SELECT t0 FROM run), 'window_s', (SELECT window_s FROM run), 'burst_at_s', (SELECT burst_at_s FROM run),
  'sets', (SELECT json_object_agg(set, to_jsonb(agg) - 'set' - 'in_burst_minute') FROM agg WHERE set IS NOT NULL),
  'overall', (SELECT to_jsonb(agg) - 'set' - 'in_burst_minute' FROM agg WHERE set IS NULL AND in_burst_minute IS NULL),
  'spread_in_burst_minute', (SELECT to_jsonb(agg) - 'set' - 'in_burst_minute' FROM agg WHERE in_burst_minute IS TRUE),
  'spread_outside_burst_minute', (SELECT to_jsonb(spread_outside) FROM spread_outside),
  'per_minute', (SELECT json_agg(to_jsonb(per_minute) ORDER BY set, minute) FROM per_minute),
  'first_step_started', (SELECT min(started_at) FROM late),
  'last_step_started', (SELECT max(started_at) FROM late),
  'dead_wake_jobs', (SELECT count(*) FROM jobs j JOIN r ON r.job_id = j.id WHERE j.state = 'dead'),
  'jobs_left', (SELECT count(*) FROM jobs j JOIN r ON r.job_id = j.id),
  'enrollments_by_state', (SELECT json_object_agg(state, c) FROM (
      SELECT e.state, count(*) AS c FROM enrollments e JOIN r ON r.enrollment_id = e.id GROUP BY e.state) s)
);
