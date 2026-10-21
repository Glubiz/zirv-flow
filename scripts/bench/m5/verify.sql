-- Validates a seeded run (seed.sh, step verify): the counts and the shape against the sample the
-- product wrote. Prints one line per check, `OK <check> <detail>` or `FAIL <check> <detail>`.
-- psql variables: ws, spread_flow, burst_flow, run, n_spread, n_burst, margin (seconds).
WITH r AS (SELECT * FROM m5bench.rows WHERE run = :run),
run AS (SELECT * FROM m5bench.runs WHERE run = :run),
checks(check_name, got, want) AS (
  -- Counts: one row of each table per seeded enrollment, three decisions each.
  SELECT 'staged ' || set, count(*), CASE set WHEN 'spread' THEN :n_spread ELSE :n_burst END FROM r GROUP BY set
  UNION ALL
  SELECT 'enrollments waiting ' || r.set, count(*), CASE r.set WHEN 'spread' THEN :n_spread ELSE :n_burst END
    FROM enrollments e JOIN r ON r.enrollment_id = e.id
    WHERE e.state = 'waiting' AND e.node = 'w1' AND e.state_version = 1 AND e.wake_at = r.wake_at
      AND e.flow_id = CASE r.set WHEN 'spread' THEN :'spread_flow'::uuid ELSE :'burst_flow'::uuid END
    GROUP BY r.set
  UNION ALL
  SELECT 'step_executions waiting ' || r.set, count(*), CASE r.set WHEN 'spread' THEN :n_spread ELSE :n_burst END
    FROM step_executions s JOIN r ON r.enrollment_id = s.enrollment_id
    WHERE s.node_id = 'w1' AND s.outcome = 'waiting' GROUP BY r.set
  UNION ALL
  SELECT 'wake jobs ready ' || r.set, count(*), CASE r.set WHEN 'spread' THEN :n_spread ELSE :n_burst END
    FROM jobs j JOIN r ON r.job_id = j.id
    WHERE j.kind = 'flow.step' AND j.state = 'ready' AND j.attempts = 0 AND j.run_at = r.wake_at
      AND j.unique_key = 'wake:' || r.enrollment_id || ':1' AND j.payload ->> 'input' = 'wake'
      AND j.payload ->> 'enrollment_id' = r.enrollment_id::text AND j.trace_id = r.trace_id
    GROUP BY r.set
  UNION ALL
  SELECT 'flow_entries ' || r.set, count(*), CASE r.set WHEN 'spread' THEN :n_spread ELSE :n_burst END
    FROM flow_entries f JOIN r ON r.person_id = f.person_id
    WHERE f.flow_id = CASE r.set WHEN 'spread' THEN :'spread_flow'::uuid ELSE :'burst_flow'::uuid END AND f.entries = 1
    GROUP BY r.set
  UNION ALL
  SELECT 'decisions x3 ' || r.set, count(*), 3 * CASE r.set WHEN 'spread' THEN :n_spread ELSE :n_burst END
    FROM decision_records d JOIN r ON r.trace_id = d.trace_id AND d.workspace_id = :'ws'
    GROUP BY r.set
  UNION ALL
  -- The wake instants: the spread inside the window, the burst inside one minute.
  SELECT 'spread wakes in window', count(*) FILTER (WHERE r.wake_at >= run.t0 AND r.wake_at < run.t0 + run.window_s * interval '1 second'),
         :n_spread FROM r, run WHERE r.set = 'spread'
  UNION ALL
  SELECT 'burst wakes in one minute', count(*) FILTER (WHERE r.wake_at >= run.t0 + run.burst_at_s * interval '1 second'
                                                          AND r.wake_at < run.t0 + (run.burst_at_s + 60) * interval '1 second'),
         :n_burst FROM r, run WHERE r.set = 'burst'
  UNION ALL
  -- The shape: every column of a seeded row equals the sample's except the per-row ones.
  SELECT 'enrollment shape ' || r.set,
         count(*) FILTER (WHERE (to_jsonb(e) - '{id,person_id,occurrence_id,trace_id,wake_at,created_at,updated_at,context}'::text[])
                              <> (t.row - '{id,person_id,occurrence_id,trace_id,wake_at,created_at,updated_at,context}'::text[])
                          OR (SELECT array_agg(k ORDER BY k) FROM jsonb_object_keys(e.context) k)
                              <> (SELECT array_agg(k ORDER BY k) FROM jsonb_object_keys(t.row -> 'context') k)
                          OR e.context ->> 'occurrence' <> e.occurrence_id::text),
         0
    FROM enrollments e JOIN r ON r.enrollment_id = e.id JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'enrollment' GROUP BY r.set
  UNION ALL
  SELECT 'step shape ' || r.set,
         count(*) FILTER (WHERE (to_jsonb(s) - '{id,enrollment_id,started_at}'::text[]) <> (t.row - '{id,enrollment_id,started_at}'::text[])), 0
    FROM step_executions s JOIN r ON r.enrollment_id = s.enrollment_id JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'step' GROUP BY r.set
  UNION ALL
  SELECT 'job shape ' || r.set,
         count(*) FILTER (WHERE (to_jsonb(j) - '{id,run_at,created_at,trace_id,payload,unique_key}'::text[])
                              <> (t.row - '{id,run_at,created_at,trace_id,payload,unique_key}'::text[])
                          OR (j.payload - 'enrollment_id') <> ((t.row -> 'payload') - 'enrollment_id')), 0
    FROM jobs j JOIN r ON r.job_id = j.id JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'job' GROUP BY r.set
  UNION ALL
  SELECT 'decision shape ' || r.set,
         count(*) FILTER (WHERE (to_jsonb(d) - '{k,id,trace_id,recorded_at,job_id,subject_ids}'::text[])
                              <> (t.row - '{id,trace_id,recorded_at,job_id,subject_ids}'::text[])
                          OR (SELECT array_agg(k ORDER BY k) FROM jsonb_object_keys(d.subject_ids) k)
                              <> (SELECT array_agg(k ORDER BY k) FROM jsonb_object_keys(t.row -> 'subject_ids') k)
                          OR (d.job_id IS NULL) <> (t.row -> 'job_id' = 'null'::jsonb)), 0
    FROM (SELECT d.*, row_number() OVER (PARTITION BY d.trace_id ORDER BY d.recorded_at, d.action) AS k
          FROM decision_records d WHERE d.workspace_id = :'ws' AND d.trace_id IN (SELECT trace_id FROM r)) d
    JOIN r ON r.trace_id = d.trace_id JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'decision' AND t.k = d.k GROUP BY r.set
  UNION ALL
  -- The window must still be ahead.
  SELECT 'window opens at least ' || :margin || ' s ahead',
         (extract(epoch FROM run.t0 - now()) >= :margin)::int, 1 FROM run
)
SELECT CASE WHEN got = want THEN 'OK' ELSE 'FAIL' END || ' ' || check_name || ' got=' || got || ' want=' || want
FROM checks ORDER BY check_name;
