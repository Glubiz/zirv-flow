-- Metrics of one flow-benchmark window, from the database (scripts/bench/flows.sh). Reads only.
-- Prints one JSON document. Scope: THIS window's trigger events, which are the events named
-- :trig that arrived at or after :since (database clock, taken before the generators started);
-- the enrollments of those events are found by enrollments.occurrence_id = events.id, so another
-- window's rows are never counted (the runner waits until a window has finished before the next).
-- psql variables: ws, flow, since, t1 (the generators' stop, database clock), secs (generator
-- seconds), wait_s (the flow's wait), trig (event name), prefix (messageId prefix of this window).
--
-- Columns used and why:
--   events.received_at           server-stamped arrival of the trigger event (items carry no timestamp)
--   enrollments.created_at       transaction start of the flow.enroll job that enrolled the person
--   enrollments.ended_at         set when the enrollment completes (the clock of the last step)
--   step_executions.started_at   transaction start of the flow.step job that ran the node; every
--                                node run by one job shares it. There is no finish column.
--   flowbench.wakes.wake_at      jobs.run_at of the wake job, sampled while it was queued; when
--                                the sampler missed it, the wait node's started_at + wait_s.
--   decision_records             `flow.enroll` decisions: outcome 'skipped' (reason) is a refusal,
--                                'deferred' would be the warm-up (must be 0).
WITH ev AS (
  SELECT id, received_at, message_id FROM events
  WHERE workspace_id = :'ws'::uuid AND name = :'trig' AND received_at >= :'since'::timestamptz
),
enr AS (
  SELECT e.id, e.state, e.created_at, e.ended_at, ev.received_at AS arrived
  FROM enrollments e JOIN ev ON ev.id = e.occurrence_id
  WHERE e.flow_id = :'flow'::uuid
),
st AS (
  SELECT s.enrollment_id,
         min(s.started_at) FILTER (WHERE s.node_id = 'b1')      AS t_b1,
         min(s.started_at) FILTER (WHERE s.node_id = 'a_trait') AS t_trait,
         min(s.started_at) FILTER (WHERE s.node_id = 'a_list')  AS t_list,
         min(s.started_at) FILTER (WHERE s.node_id = 'w1')      AS t_w1,
         min(s.started_at) FILTER (WHERE s.node_id = 'a_after') AS t_after,
         min(s.started_at) FILTER (WHERE s.node_id = 'x1')      AS t_x1
  FROM step_executions s JOIN enr ON enr.id = s.enrollment_id
  GROUP BY s.enrollment_id
),
j AS (
  SELECT enr.id, enr.state, enr.created_at, enr.ended_at, enr.arrived,
         st.t_b1, st.t_trait, st.t_list, st.t_w1, st.t_after, st.t_x1,
         coalesce(w.wake_at, st.t_w1 + make_interval(secs => :wait_s)) AS wake_at,
         (w.wake_at IS NOT NULL) AS wake_captured
  FROM enr LEFT JOIN st ON st.enrollment_id = enr.id
           LEFT JOIN flowbench.wakes w ON w.enrollment_id = enr.id
),
stats AS (
  SELECT 'trigger_to_enrollment' AS m, extract(epoch FROM created_at - arrived)::float8 AS v FROM j
  UNION ALL SELECT 'end_to_end', extract(epoch FROM ended_at - arrived)::float8 FROM j WHERE state = 'completed'
  UNION ALL SELECT 'end_to_end_minus_wait', extract(epoch FROM ended_at - arrived)::float8 - :wait_s FROM j WHERE state = 'completed'
  UNION ALL SELECT 'wake_lateness', extract(epoch FROM t_after - wake_at)::float8 FROM j WHERE t_after IS NOT NULL AND wake_at IS NOT NULL
  UNION ALL SELECT 'wake_lateness_captured_only', extract(epoch FROM t_after - wake_at)::float8 FROM j WHERE t_after IS NOT NULL AND wake_captured
  UNION ALL SELECT 'queue:b1', extract(epoch FROM t_b1 - created_at)::float8 FROM j
  UNION ALL SELECT 'queue:a_trait', extract(epoch FROM t_trait - t_b1)::float8 FROM j
  UNION ALL SELECT 'queue:a_list', extract(epoch FROM t_list - t_b1)::float8 FROM j
  UNION ALL SELECT 'queue:w1', extract(epoch FROM t_w1 - coalesce(t_trait, t_list))::float8 FROM j
  UNION ALL SELECT 'queue:a_after', extract(epoch FROM t_after - wake_at)::float8 FROM j WHERE wake_at IS NOT NULL
  UNION ALL SELECT 'queue:x1', extract(epoch FROM t_x1 - t_after)::float8 FROM j
),
agg AS (
  SELECT m, count(v) AS n,
         round(percentile_disc(0.50) WITHIN GROUP (ORDER BY v)::numeric, 3) AS p50,
         round(percentile_disc(0.95) WITHIN GROUP (ORDER BY v)::numeric, 3) AS p95,
         round(percentile_disc(0.99) WITHIN GROUP (ORDER BY v)::numeric, 3) AS p99,
         round(max(v)::numeric, 3) AS max,
         round(min(v)::numeric, 3) AS min
  FROM stats WHERE v IS NOT NULL GROUP BY m
),
refused AS (
  SELECT coalesce(reason, 'none') AS reason, count(*) AS c FROM decision_records
  WHERE workspace_id = :'ws'::uuid AND action = 'flow.enroll' AND outcome = 'skipped'
    AND recorded_at >= :'since'::timestamptz AND subject_ids ->> 'flow' = :'flow'
  GROUP BY 1
),
by_state AS (SELECT state, count(*) AS c FROM enr GROUP BY state)
SELECT json_build_object(
  'since', :'since', 'generators_stopped', :'t1',
  'counts', json_build_object(
    'trigger_events_stored', (SELECT count(*) FROM ev),
    'trigger_events_with_run_prefix', (SELECT count(*) FROM ev WHERE message_id LIKE :'prefix' || '%'),
    'enrollments_created', (SELECT count(*) FROM enr),
    'enrollments_by_state', (SELECT coalesce(json_object_agg(state, c), '{}'::json) FROM by_state),
    'completed', (SELECT count(*) FROM enr WHERE state = 'completed'),
    'failed', (SELECT count(*) FROM enr WHERE state = 'failed'),
    'still_active_or_waiting', (SELECT count(*) FROM enr WHERE state IN ('active', 'waiting')),
    'refused_by_reason', (SELECT coalesce(json_object_agg(reason, c), '{}'::json) FROM refused),
    'refused_total', (SELECT coalesce(sum(c), 0) FROM refused),
    'refused_reentry', (SELECT coalesce(sum(c), 0) FROM refused WHERE reason IN ('already_active', 'already_entered', 'reentry_interval')),
    'deferred_by_warmup', (SELECT count(*) FROM decision_records WHERE workspace_id = :'ws'::uuid AND action = 'flow.enroll'
        AND outcome = 'deferred' AND recorded_at >= :'since'::timestamptz AND subject_ids ->> 'flow' = :'flow'),
    'dead_jobs_since', (SELECT count(*) FROM jobs WHERE state = 'dead' AND dead_at >= :'since'::timestamptz),
    'wake_times_captured', (SELECT count(*) FROM j WHERE wake_captured),
    'wake_times_estimated', (SELECT count(*) FROM j WHERE NOT wake_captured AND t_w1 IS NOT NULL)),
  'seconds', (SELECT coalesce(json_object_agg(m, to_jsonb(agg) - 'm'), '{}'::json) FROM agg),
  'throughput', json_build_object(
    'generator_window_s', :secs,
    'enrollments_created_in_window', (SELECT count(*) FROM enr WHERE created_at <= :'t1'::timestamptz),
    'enrollments_per_s', round((SELECT count(*) FROM enr WHERE created_at <= :'t1'::timestamptz)::numeric / :secs, 2),
    'step_executions_in_window', (SELECT count(*) FROM step_executions s JOIN enr ON enr.id = s.enrollment_id
                                  WHERE s.started_at <= :'t1'::timestamptz),
    'step_executions_per_s', round((SELECT count(*) FROM step_executions s JOIN enr ON enr.id = s.enrollment_id
                                  WHERE s.started_at <= :'t1'::timestamptz)::numeric / :secs, 2),
    'step_executions_total', (SELECT count(*) FROM step_executions s JOIN enr ON enr.id = s.enrollment_id),
    'span_first_enrollment_to_last_end_s', (SELECT round(extract(epoch FROM max(ended_at) - min(created_at))::numeric, 1) FROM enr)),
  'step_outcomes', (SELECT coalesce(json_object_agg(node_id || '/' || outcome, c), '{}'::json) FROM (
      SELECT s.node_id, s.outcome, count(*) AS c FROM step_executions s JOIN enr ON enr.id = s.enrollment_id
      GROUP BY s.node_id, s.outcome) o)
);
