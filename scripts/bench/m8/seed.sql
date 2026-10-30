-- Only the disposable M8 database. Helper tables stay here, never in migrations/DELETIONS.
\set ON_ERROR_STOP on
SET timezone = 'UTC';
DO $$ BEGIN
  IF current_database() <> 'zirv_flow_m8' THEN RAISE EXCEPTION 'M8 bench database required'; END IF;
END $$;
CREATE SCHEMA IF NOT EXISTS m8bench;
CREATE TABLE IF NOT EXISTS m8bench.config (
  singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  sizes jsonb NOT NULL, seed_at timestamptz NOT NULL DEFAULT now(),
  wake_at timestamptz NOT NULL DEFAULT now() + interval '7 days'
);
CREATE TABLE IF NOT EXISTS m8bench.progress (
  kind text NOT NULL, workspace_id uuid NOT NULL, last_n bigint NOT NULL DEFAULT 0,
  PRIMARY KEY (kind, workspace_id)
);
INSERT INTO m8bench.config (sizes)
VALUES (jsonb_build_object('people', :people::bigint, 'events', :events::bigint,
                          'enrollments', :enrollments::bigint)) ON CONFLICT DO NOTHING;
SELECT sizes = jsonb_build_object('people', :people::bigint, 'events', :events::bigint,
                                 'enrollments', :enrollments::bigint) AS same_scale
FROM m8bench.config \gset
\if :same_scale
\else
  \echo 'M8 scale differs from this volume: resume with the original sizes or use a fresh bench volume'
  SELECT 1/0;
\endif

CREATE OR REPLACE FUNCTION m8bench.size(total bigint, slot integer) RETURNS bigint
LANGUAGE sql IMMUTABLE AS $$
  SELECT CASE WHEN slot < 3 THEN total * 2 / 5 ELSE total - 2 * (total * 2 / 5) END
$$;
CREATE OR REPLACE FUNCTION m8bench.ws(slot integer) RETURNS uuid
LANGUAGE sql IMMUTABLE AS $$ SELECT md5('m8/workspace/' || slot)::uuid $$;
INSERT INTO workspaces (id, name, slug)
SELECT m8bench.ws(g), 'm8-bench-' || g, 'm8-bench-' || g FROM generate_series(1, 3) g
ON CONFLICT (id) DO NOTHING;

-- The exact M4/OQ-1 catalog; m4/seed.sql supplies the original 20-trait generator.
INSERT INTO trait_definitions (workspace_id, key, type)
SELECT m8bench.ws(g), t.key, t.type FROM generate_series(1, 3) g CROSS JOIN (VALUES
  ('country','string'), ('plan','string'), ('lifecycle','string'), ('channel','string'),
  ('city','string'), ('region','string'), ('age','number'), ('ltv','number'), ('seats','number'),
  ('orders','number'), ('score','number'), ('marketing_opt_in','boolean'),
  ('email_verified','boolean'), ('is_vip','boolean'), ('trial','boolean'),
  ('signup_at','timestamp'), ('last_seen_at','timestamp'), ('birthday','date'),
  ('tags','string_list'), ('interests','string_list')) t(key, type)
ON CONFLICT DO NOTHING;
INSERT INTO event_schemas (workspace_id, name, mode, current_version)
SELECT m8bench.ws(g), name, 'warn', 1 FROM generate_series(1, 3) g CROSS JOIN unnest(ARRAY[
  'bench_page_viewed','bench_item_added','bench_order_placed','bench_plan_changed','bench_support_opened']) name
ON CONFLICT DO NOTHING;
INSERT INTO event_schema_versions (workspace_id, name, version, properties)
SELECT workspace_id, name, 1,
  '[{"name":"amount","type":"number"},{"name":"quantity","type":"integer"},{"name":"bench_seq","type":"integer"}]'::jsonb
  || CASE WHEN name = 'bench_order_placed' THEN '[{"name":"category","type":"string"}]'::jsonb ELSE '[]'::jsonb END
FROM event_schemas WHERE workspace_id IN (SELECT m8bench.ws(g) FROM generate_series(1, 3) g)
ON CONFLICT DO NOTHING;

-- Ensure every month in the persisted 365-day window has a real partition before loading.
SELECT format('CREATE TABLE IF NOT EXISTS %I PARTITION OF events FOR VALUES FROM (%L) TO (%L)',
              to_char(m, '"events_y"YYYY"m"MM'), m, m + interval '1 month')
FROM m8bench.config, LATERAL generate_series(date_trunc('month', seed_at - interval '365 days'),
                                            date_trunc('month', seed_at), interval '1 month') m
\gexec

\set seed_functions_only true
\ir ../m4/seed.sql
CREATE FUNCTION pg_temp.seed_chunk(kind text, ws uuid, lo bigint, hi bigint, n_people bigint)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE done bigint; anchor timestamptz;
BEGIN
  PERFORM pg_advisory_xact_lock(89393, 0);
  INSERT INTO m8bench.progress VALUES (kind, ws, 0) ON CONFLICT DO NOTHING;
  SELECT last_n INTO done FROM m8bench.progress p WHERE p.kind = seed_chunk.kind AND p.workspace_id = ws FOR UPDATE;
  IF done >= hi THEN RETURN; END IF;
  IF done <> lo - 1 THEN RAISE EXCEPTION 'non-contiguous seed batch'; END IF;
  SELECT seed_at INTO anchor FROM m8bench.config;
  PERFORM setseed(hashtext(ws::text || '/' || lo)::double precision / 2147483648.0);
  IF kind = 'people' THEN
    PERFORM pg_temp.seed_people(ws, lo, hi, anchor);
    INSERT INTO identities (id, workspace_id, person_id, kind, value)
    SELECT md5(ws::text || '/i/' || g)::uuid, ws, md5(ws::text || '/p/' || g)::uuid,
           'external:user_id', 'bench-u-' || (g - 1)
    FROM generate_series(lo, least(hi, 1000000)) g;
  ELSIF kind = 'events' THEN
    PERFORM pg_temp.seed_events(ws, lo, hi, n_people, 365, anchor);
  END IF;
  UPDATE m8bench.progress p SET last_n = hi WHERE p.kind = seed_chunk.kind AND p.workspace_id = ws;
END $$;
SELECT :'kind' = 'enrollments' AS timers \gset
\if :timers
  \set seed_schema_only true
  \ir ../m5/seed-stage.sql
  -- Reuse the M5 product-path sample as the loader template, remapping all workspace/flow ids.
  -- No API is needed to make the samples. Current NOT NULL columns were checked against 0036/0039/0041.
  CREATE FUNCTION pg_temp.timer_templates(ws uuid, sample jsonb) RETURNS void LANGUAGE plpgsql AS $$
  DECLARE flow uuid := md5(ws::text || '/flow')::uuid; e jsonb; row_data jsonb; entry record;
    graph jsonb := '{"v":1,"nodes":[{"id":"w1","type":"wait.duration","seconds":86400},{"id":"x1","type":"exit"}],"edges":[{"id":"e1","from":"w1","port":"next","to":"x1"}]}';
    settings jsonb := '{"trigger":{"type":"manual"},"reentry":{"mode":"once"}}';
  BEGIN
    INSERT INTO flows (id, workspace_id, name, state, draft_graph, draft_settings, current_version)
    VALUES (flow, ws, 'm8-spread', 'published', graph, settings, 1) ON CONFLICT DO NOTHING;
    INSERT INTO flow_versions (workspace_id, flow_id, version, graph, settings, trace_id)
    VALUES (ws, flow, 1, graph, settings, flow) ON CONFLICT DO NOTHING;
    e := sample->'enrollments'->0;
    FOR entry IN
      SELECT 'enrollment' AS kind, 1::bigint AS k, e AS row
      UNION ALL SELECT 'step', 1, v FROM jsonb_array_elements(sample->'step_executions') v WHERE v->>'enrollment_id' = e->>'id'
      UNION ALL SELECT 'job', 1, v FROM jsonb_array_elements(sample->'jobs') v WHERE v->'payload'->>'enrollment_id' = e->>'id'
      UNION ALL SELECT 'flow_entry', 1, v FROM jsonb_array_elements(sample->'flow_entries') v WHERE v->>'person_id' = e->>'person_id'
      UNION ALL SELECT 'decision', row_number() OVER (ORDER BY v->>'action'), v
        FROM jsonb_array_elements(sample->'decision_records') v WHERE v->>'trace_id' = e->>'trace_id'
    LOOP
      row_data := entry.row || jsonb_build_object('workspace_id', ws, 'cause_id', flow);
      IF row_data ? 'flow_id' THEN row_data := row_data || jsonb_build_object('flow_id', flow); END IF;
      IF row_data ? 'payload' THEN row_data := row_data || jsonb_build_object('payload', (row_data->'payload') || jsonb_build_object('workspace_id', ws)); END IF;
      IF row_data ? 'subject_ids' THEN row_data := row_data || jsonb_build_object('subject_ids', (row_data->'subject_ids') || jsonb_build_object('flow', flow)); END IF;
      INSERT INTO m5bench.tmpl (set, kind, k, row) VALUES (ws::text, entry.kind, entry.k, row_data)
      ON CONFLICT DO NOTHING;
    END LOOP;
    IF (SELECT count(*) FROM m5bench.tmpl WHERE set = ws::text) <> 7 THEN
      RAISE EXCEPTION 'expected enrollment, step, job, entry and three decision templates';
    END IF;
  END $$;
  SELECT pg_temp.timer_templates(m8bench.ws(g), :'sample'::jsonb) FROM generate_series(1, 3) g;
\else
  SELECT format('SELECT pg_temp.seed_chunk(%L, %L, %s, %s, %s)', :'kind', m8bench.ws(g),
                lo, least(lo + :chunk - 1, m8bench.size((sizes->>:'kind')::bigint, g)),
                m8bench.size((sizes->>'people')::bigint, g))
  FROM m8bench.config CROSS JOIN generate_series(1, 3) g
  LEFT JOIN m8bench.progress p ON p.kind = :'kind' AND p.workspace_id = m8bench.ws(g)
  CROSS JOIN LATERAL generate_series(coalesce(p.last_n, 0) + 1, m8bench.size((sizes->>:'kind')::bigint, g), :chunk) lo
  ORDER BY g, lo
  \gexec
\endif
