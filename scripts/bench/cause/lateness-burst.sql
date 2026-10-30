-- #207 cause analysis: lateness of the M5/M8 wake timers split by the burst minute (wake instants
-- t0+300 s .. t0+360 s) and the rest. Same definition as scripts/bench/m5/lateness.sql. One JSON document.
WITH run AS (SELECT * FROM m5bench.runs WHERE run = 1),
late AS (
  SELECT extract(epoch FROM s.started_at - r.wake_at) AS late_s,
         (r.wake_at >= run.t0 + interval '300 seconds' AND r.wake_at < run.t0 + interval '360 seconds') AS in_burst
  FROM m5bench.rows r CROSS JOIN run
  LEFT JOIN step_executions s ON s.enrollment_id = r.enrollment_id AND s.node_id = 'x1'
  WHERE r.run = 1
),
agg AS (
  SELECT in_burst, count(*) AS seeded, count(late_s) AS woken,
         percentile_disc(0.5) WITHIN GROUP (ORDER BY late_s) AS p50_s,
         percentile_disc(0.95) WITHIN GROUP (ORDER BY late_s) AS p95_s,
         percentile_disc(0.99) WITHIN GROUP (ORDER BY late_s) AS p99_s,
         max(late_s) AS max_s
  FROM late GROUP BY GROUPING SETS ((in_burst), ())
)
SELECT json_build_object(
  'all', (SELECT to_jsonb(agg) - 'in_burst' FROM agg WHERE in_burst IS NULL),
  'burst_set', (SELECT to_jsonb(agg) - 'in_burst' FROM agg WHERE in_burst IS TRUE),
  'spread_set', (SELECT to_jsonb(agg) - 'in_burst' FROM agg WHERE in_burst IS FALSE));
