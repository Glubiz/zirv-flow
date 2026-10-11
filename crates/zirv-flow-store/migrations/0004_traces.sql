-- Traceability: every job carries the trace and cause it started from, and the decision
-- log records what happened under a trace (architecture sections 12.4 and 17).

-- Jobs that exist before this migration have no recorded origin: each gets a trace of
-- its own and the cause (manual, ''). The defaults only serve the backfill.
ALTER TABLE jobs
    ADD COLUMN trace_id uuid NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN cause_kind text NOT NULL DEFAULT 'manual'
        CHECK (cause_kind IN ('event', 'trait_change', 'schedule', 'manual', 'api', 'definition_change')),
    ADD COLUMN cause_id text NOT NULL DEFAULT '';
ALTER TABLE jobs
    ALTER COLUMN trace_id DROP DEFAULT,
    ALTER COLUMN cause_kind DROP DEFAULT,
    ALTER COLUMN cause_id DROP DEFAULT;

-- Append-only: the store offers no update or delete. Partitioned by month so retention
-- can later drop whole partitions. The DEFAULT partition catches a row whose month has no
-- partition yet, so an insert never fails.
CREATE TABLE decision_records (
    id           uuid NOT NULL,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    trace_id     uuid NOT NULL,
    recorded_at  timestamptz NOT NULL DEFAULT now(),
    action       text NOT NULL,
    outcome      text NOT NULL,
    reason       text,
    cause_kind   text NOT NULL
        CHECK (cause_kind IN ('event', 'trait_change', 'schedule', 'manual', 'api', 'definition_change')),
    cause_id     text NOT NULL,
    -- Durable domain ids only, as name to uuid; never personal data.
    subject_ids  jsonb NOT NULL DEFAULT '{}'::jsonb,
    job_id       uuid,
    attempt      integer,
    PRIMARY KEY (recorded_at, id)
) PARTITION BY RANGE (recorded_at);

CREATE TABLE decision_records_default PARTITION OF decision_records DEFAULT;

CREATE INDEX decision_records_trace_idx ON decision_records (workspace_id, trace_id, recorded_at);

-- The current and the next UTC month get their partitions now, so decisions written before
-- any scheduler leader has run do not land in DEFAULT. Names and bounds match the leader's
-- upkeep in the store crate, which then finds them existing.
DO $$
DECLARE
    month_start timestamptz;
BEGIN
    FOREACH month_start IN ARRAY ARRAY[
        date_trunc('month', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC',
        (date_trunc('month', now() AT TIME ZONE 'UTC') + interval '1 month') AT TIME ZONE 'UTC'
    ] LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS decision_records_y%sm%s PARTITION OF decision_records '
            'FOR VALUES FROM (%L) TO (%L)',
            to_char(month_start AT TIME ZONE 'UTC', 'YYYY'),
            to_char(month_start AT TIME ZONE 'UTC', 'MM'),
            to_char(month_start AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') || '+00',
            to_char(month_start AT TIME ZONE 'UTC' + interval '1 month', 'YYYY-MM-DD HH24:MI:SS') || '+00'
        );
    END LOOP;
END
$$;
