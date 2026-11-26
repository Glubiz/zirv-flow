-- One sample of the queue, the inbox and the flow's enrollments, as one JSON line.
-- psql variable: flow (the flow id).
SELECT json_build_object(
  'db_epoch', extract(epoch FROM now())::bigint,
  'due_ready', (SELECT count(*) FROM jobs WHERE state = 'ready' AND run_at <= now()),
  'due_ready_by_kind', (SELECT coalesce(json_object_agg(kind, c), '{}'::json) FROM (
      SELECT kind, count(*) AS c FROM jobs WHERE state = 'ready' AND run_at <= now() GROUP BY kind) k),
  'running', (SELECT count(*) FROM jobs WHERE state = 'running'),
  'oldest_due_age_s', (SELECT round(extract(epoch FROM now() - min(run_at))::numeric, 1)
                       FROM jobs WHERE state = 'ready' AND run_at <= now()),
  'inbox_rows', (SELECT count(*) FROM ingest_inbox),
  'inbox_oldest_age_s', (SELECT round(extract(epoch FROM now() - min(received_at))::numeric, 1) FROM ingest_inbox),
  'enrollments', (SELECT coalesce(json_object_agg(state, c), '{}'::json) FROM (
      SELECT state, count(*) AS c FROM enrollments WHERE flow_id = :'flow' GROUP BY state) s)
);
