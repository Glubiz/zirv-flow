-- Stage one seed run (scripts/bench/m5/seed.sh, step stage). psql variables: ws, spread_flow,
-- burst_flow, run, t0 (the instant the measurement window opens), window_s, burst_at_s, n_spread,
-- n_burst. Everything lands in the helper schema m5bench; no product table is touched here.
--
--   m5bench.tmpl   the rows the product itself wrote for a sample enrollment of each flow
--                  (sample.sh), one JSON document per table row: the template of the seed
--   m5bench.rows   one row per seeded enrollment: its ids, person, wake instant and timestamps.
--                  It is also the lateness query's source of `wake_at`, which the runtime clears
--                  when the enrollment completes.
--
-- Spread set: wake_at = t0 + uniform(0, window_s) seconds. Burst set: wake_at = t0 + burst_at_s +
-- uniform(0, 60) seconds, so all the burst lands in one minute.
CREATE SCHEMA IF NOT EXISTS m5bench;

-- A version 7 uuid for an instant: the first 48 bits are the milliseconds since the epoch (as the
-- runtime's Uuid::now_v7), the rest random, version and variant bits set.
CREATE OR REPLACE FUNCTION m5bench.uuid7(ts timestamptz) RETURNS uuid LANGUAGE sql VOLATILE AS $$
  SELECT encode(
    set_byte(
      overlay(uuid_send(gen_random_uuid()) PLACING substring(int8send((extract(epoch FROM ts) * 1000)::bigint) FROM 3 FOR 6) FROM 1 FOR 6),
      6, (get_byte(uuid_send(gen_random_uuid()), 6) & 15) | 112),
    'hex')::uuid
$$;

CREATE TABLE IF NOT EXISTS m5bench.tmpl (
  set  text  NOT NULL,
  kind text  NOT NULL,
  k    int   NOT NULL DEFAULT 1,
  row  jsonb NOT NULL,
  PRIMARY KEY (set, kind, k)
);
CREATE TABLE IF NOT EXISTS m5bench.rows (
  run           int         NOT NULL,
  set           text        NOT NULL,
  n             int         NOT NULL,
  person_id     uuid        NOT NULL,
  enrollment_id uuid        NOT NULL,
  occurrence_id uuid        NOT NULL,
  trace_id      uuid        NOT NULL,
  step_id       uuid        NOT NULL,
  job_id        uuid        NOT NULL,
  start_job_id  uuid        NOT NULL,
  created_at    timestamptz NOT NULL,
  step_at       timestamptz NOT NULL,
  wake_at       timestamptz NOT NULL,
  PRIMARY KEY (run, set, n)
);
CREATE INDEX IF NOT EXISTS rows_enrollment_idx ON m5bench.rows (enrollment_id);
-- One row per run: when the window opens and what was asked for.
CREATE TABLE IF NOT EXISTS m5bench.runs (
  run int PRIMARY KEY, t0 timestamptz NOT NULL, window_s int NOT NULL, burst_at_s int NOT NULL,
  n_spread int NOT NULL, n_burst int NOT NULL, staged_at timestamptz NOT NULL DEFAULT now()
);

-- The templates: the oldest sample enrollment of each flow (a person among the first 100 of the
-- workspace, which the seed never uses), with its step row, wake job, flow entry and decisions.
DELETE FROM m5bench.tmpl;
WITH sample AS (
  SELECT id FROM people WHERE workspace_id = :'ws' AND merged_into IS NULL ORDER BY id LIMIT 100
), f(set, flow_id) AS (VALUES ('spread', :'spread_flow'::uuid), ('burst', :'burst_flow'::uuid)),
t AS (
  SELECT DISTINCT ON (f.set) f.set, e.* FROM f
  JOIN enrollments e ON e.flow_id = f.flow_id AND e.person_id IN (SELECT id FROM sample)
  ORDER BY f.set, e.created_at
)
INSERT INTO m5bench.tmpl (set, kind, k, row)
SELECT set, 'enrollment', 1, to_jsonb(t) - 'set' FROM t
UNION ALL
SELECT t.set, 'step', 1, to_jsonb(s) FROM t JOIN step_executions s ON s.enrollment_id = t.id
UNION ALL
SELECT t.set, 'job', 1, to_jsonb(j) FROM t JOIN jobs j ON j.kind = 'flow.step' AND j.payload ->> 'enrollment_id' = t.id::text
UNION ALL
SELECT t.set, 'flow_entry', 1, to_jsonb(fe) FROM t JOIN flow_entries fe ON fe.flow_id = t.flow_id AND fe.person_id = t.person_id
UNION ALL
SELECT set, 'decision', row_number() OVER (PARTITION BY set ORDER BY recorded_at, action), to_jsonb(d) - 'set'
FROM (SELECT t.set, d.* FROM t JOIN decision_records d ON d.workspace_id = t.workspace_id AND d.trace_id = t.trace_id) d;

DELETE FROM m5bench.rows WHERE run = :run;
DELETE FROM m5bench.runs WHERE run = :run;
INSERT INTO m5bench.runs (run, t0, window_s, burst_at_s, n_spread, n_burst)
VALUES (:run, :'t0'::timestamptz, :window_s, :burst_at_s, :n_spread, :n_burst);

CREATE TEMP TABLE pool AS
SELECT id, row_number() OVER (ORDER BY id) AS rn
FROM (SELECT id FROM people WHERE workspace_id = :'ws' AND merged_into IS NULL
      ORDER BY id OFFSET 100 LIMIT (:n_spread + :n_burst)) p;

INSERT INTO m5bench.rows
SELECT :run, g.set, g.n, p.id,
       m5bench.uuid7(g.created), m5bench.uuid7(g.created), gen_random_uuid(),
       m5bench.uuid7(g.created + interval '13 milliseconds'), gen_random_uuid(), gen_random_uuid(),
       g.created, g.created + interval '13 milliseconds',
       CASE g.set
         WHEN 'spread' THEN :'t0'::timestamptz + random() * :window_s * interval '1 second'
         ELSE :'t0'::timestamptz + :burst_at_s * interval '1 second' + random() * 60 * interval '1 second'
       END
FROM (
  SELECT s.set, s.n, s.rn,
         now() - interval '90 minutes' + (s.rn::float8 / (:n_spread + :n_burst)) * interval '1 hour' AS created
  FROM (SELECT 'spread' AS set, i AS n, i AS rn FROM generate_series(1, :n_spread) i
        UNION ALL
        SELECT 'burst', i, :n_spread + i FROM generate_series(1, :n_burst) i) s
) g
JOIN pool p ON p.rn = g.rn;

ANALYZE m5bench.rows;
SELECT set, count(*) AS staged, min(wake_at) AS first_wake, max(wake_at) AS last_wake
FROM m5bench.rows WHERE run = :run GROUP BY set ORDER BY set;
