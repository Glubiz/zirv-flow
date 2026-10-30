-- Scratch-database fixtures: the same columns/defaults as jobs::enqueue/NewJob::new.
BEGIN;
DO $$ BEGIN
  IF current_database() <> 'zirv_flow_m8' THEN RAISE EXCEPTION 'M8 bench database required'; END IF;
  IF EXISTS (SELECT 1 FROM jobs WHERE kind = 'maintenance.noop') THEN
    RAISE EXCEPTION 'remove leftover no-op bench jobs before starting another jobs run';
  END IF;
END $$;
CREATE TABLE IF NOT EXISTS m8bench.jobs_probe (
  trace_id uuid PRIMARY KEY, quiet_id uuid NOT NULL, busy bigint NOT NULL,
  ready_at timestamptz NOT NULL, quiet_claimed_at timestamptz, quiet_claim_lag_s double precision
);
INSERT INTO m8bench.jobs_probe (trace_id, quiet_id, busy, ready_at)
VALUES (:'trace'::uuid, gen_random_uuid(), :busy, clock_timestamp() + interval '1 day');

-- A no-op may finish between samples. Instrument only the quiet row's claim, using the
-- same transaction timestamp as claim_job's EXTRACT(EPOCH FROM now() - run_at).
CREATE OR REPLACE FUNCTION m8bench.record_quiet_claim() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  UPDATE m8bench.jobs_probe SET quiet_claimed_at = now(),
    quiet_claim_lag_s = extract(epoch FROM now() - NEW.run_at)
  WHERE quiet_id = NEW.id AND quiet_claimed_at IS NULL;
  RETURN NEW;
END $$;
SELECT format('CREATE TRIGGER m8_quiet_claim AFTER UPDATE OF state ON jobs FOR EACH ROW '
  'WHEN (NEW.id = %L::uuid AND OLD.state = ''ready'' AND NEW.state = ''running'') '
  'EXECUTE FUNCTION m8bench.record_quiet_claim()', quiet_id)
FROM m8bench.jobs_probe WHERE trace_id = :'trace'::uuid
\gexec

WITH inserted AS (
  INSERT INTO jobs (id, workspace_id, kind, payload, priority, run_at, max_attempts,
                    unique_key, trace_id, cause_kind, cause_id)
  SELECT CASE WHEN g = :busy + 1 THEN p.quiet_id ELSE gen_random_uuid() END,
         m8bench.ws(CASE WHEN g = :busy + 1 THEN 2 ELSE 1 END),
         'maintenance.noop', '{}'::jsonb, 0, p.ready_at, 5, NULL, p.trace_id, 'manual', 'm8-jobs'
  FROM m8bench.jobs_probe p CROSS JOIN generate_series(1, :busy + 1) g
  WHERE p.trace_id = :'trace'::uuid
  ON CONFLICT DO NOTHING RETURNING kind
), notified AS (
  SELECT pg_notify('zirv_flow_jobs', k.kind) FROM (SELECT DISTINCT kind FROM inserted) k
  WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE state = 'ready' AND kind = k.kind AND run_at <= now())
)
SELECT count(*) AS enqueued FROM inserted, (SELECT count(*) FROM notified) n;

-- Arm after insertion, leaving time to start sampling. Equal priority, busy rows 1 us older.
UPDATE m8bench.jobs_probe SET ready_at = clock_timestamp() + :lead * interval '1 second'
WHERE trace_id = :'trace'::uuid;
UPDATE jobs j SET run_at = p.ready_at + CASE WHEN j.id = p.quiet_id THEN interval '1 microsecond' ELSE interval '0' END
FROM m8bench.jobs_probe p WHERE p.trace_id = :'trace'::uuid AND j.trace_id = p.trace_id;
SELECT ready_at > clock_timestamp() + interval '2 seconds' AS future_window
FROM m8bench.jobs_probe WHERE trace_id = :'trace'::uuid
\gset
\if :future_window
\else
  DO $$ BEGIN RAISE EXCEPTION 'enqueue exceeded lead time; increase M8_JOBS_LEAD_SECONDS'; END $$;
\endif
COMMIT;
