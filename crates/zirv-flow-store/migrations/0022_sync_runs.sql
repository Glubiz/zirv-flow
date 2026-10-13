-- Sync runs (architecture sections 7.1, 7.2, 12.4, 14 and 16.3): one run reads one stream of a
-- source from a checkpoint, in batches, and applies the records to people. A run holds counters,
-- a checkpoint and a trace id; it never holds a person or a record value. Rejected records go to
-- `ingest_rejects`, and the cron of a source is mirrored to a generic `schedules` row.

-- A run refers to its stream with the workspace in the key, so a cross-workspace link is refused.
ALTER TABLE streams ADD CONSTRAINT streams_workspace_id_id_key UNIQUE (workspace_id, id);

-- Where an incremental stream resumes in its next run; null until a run has committed a batch,
-- and never written for a full refresh.
ALTER TABLE streams ADD COLUMN checkpoint jsonb;

CREATE TABLE sync_runs (
    id                uuid PRIMARY KEY,
    workspace_id      uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    source_id         uuid NOT NULL,
    stream_id         uuid NOT NULL,
    state             text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    trigger           text NOT NULL CHECK (trigger IN ('manual', 'schedule')),
    -- The stream's sync mode when the run was created.
    sync_mode         text NOT NULL CHECK (sync_mode IN ('full_refresh', 'incremental')),
    records_read      bigint NOT NULL DEFAULT 0,
    records_applied   bigint NOT NULL DEFAULT 0,
    records_unchanged bigint NOT NULL DEFAULT 0,
    records_rejected  bigint NOT NULL DEFAULT 0,
    records_deleted   bigint NOT NULL DEFAULT 0,
    reject_threshold  double precision NOT NULL DEFAULT 0.05
        CHECK (reject_threshold >= 0 AND reject_threshold <= 1),
    -- The resume point inside this run, written with every committed batch. Opaque.
    checkpoint        jsonb,
    -- Why a failed run failed: a code, never a value.
    error_code        text,
    -- How many times a worker started the run; more than one means a lease expired.
    attempt           integer NOT NULL DEFAULT 0,
    trace_id          uuid NOT NULL,
    created_by        uuid REFERENCES users (id),
    created_at        timestamptz NOT NULL DEFAULT now(),
    started_at        timestamptz,
    finished_at       timestamptz,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, source_id) REFERENCES sources (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, stream_id) REFERENCES streams (workspace_id, id) ON DELETE CASCADE
);

-- One active run per stream, whoever starts it.
CREATE UNIQUE INDEX sync_runs_one_active_idx ON sync_runs (stream_id)
    WHERE state IN ('queued', 'running');
CREATE INDEX sync_runs_history_idx ON sync_runs (workspace_id, source_id, created_at DESC, id DESC);

-- A sync reject names its run. The run's rejects stay when the run is deleted with its source's
-- stream; the 30-day retention removes them.
ALTER TABLE ingest_rejects
    ADD COLUMN sync_run_id uuid REFERENCES sync_runs (id) ON DELETE SET NULL;
CREATE INDEX ingest_rejects_sync_run_idx ON ingest_rejects (sync_run_id, received_at, id)
    WHERE sync_run_id IS NOT NULL;

-- A source's cron, mirrored to a schedule the leader fires. Deleting the source deletes it.
ALTER TABLE schedules
    ADD COLUMN source_id uuid REFERENCES sources (id) ON DELETE CASCADE;
CREATE UNIQUE INDEX schedules_source_idx ON schedules (source_id) WHERE source_id IS NOT NULL;

-- Existing sources with a schedule fire from now on. The first slot is due at once; the leader
-- then moves it to the next slot of the cron.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled, source_id)
SELECT gen_random_uuid(), s.workspace_id, 'sync.run', s.schedule, 'UTC', now(),
       jsonb_build_object('source_id', s.id), s.enabled, s.id
FROM sources s
WHERE s.schedule IS NOT NULL AND s.schedule <> '';

-- The seventh cause: a sync run.
ALTER TABLE jobs DROP CONSTRAINT jobs_cause_kind_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_cause_kind_check
    CHECK (cause_kind IN ('event', 'trait_change', 'schedule', 'manual', 'api', 'definition_change', 'sync_run'));
ALTER TABLE decision_records DROP CONSTRAINT decision_records_cause_kind_check;
ALTER TABLE decision_records ADD CONSTRAINT decision_records_cause_kind_check
    CHECK (cause_kind IN ('event', 'trait_change', 'schedule', 'manual', 'api', 'definition_change', 'sync_run'));
