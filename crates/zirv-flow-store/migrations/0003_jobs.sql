-- The durable job queue and recurring schedules (architecture section 14).

CREATE TABLE jobs (
    id               uuid PRIMARY KEY,
    -- Null for instance-level work. A deleted workspace takes its queued jobs with it.
    workspace_id     uuid REFERENCES workspaces (id) ON DELETE CASCADE,
    kind             text NOT NULL,
    payload          jsonb NOT NULL DEFAULT '{}'::jsonb,
    priority         smallint NOT NULL DEFAULT 0,
    run_at           timestamptz NOT NULL DEFAULT now(),
    state            text NOT NULL DEFAULT 'ready' CHECK (state IN ('ready', 'running', 'dead')),
    attempts         integer NOT NULL DEFAULT 0,
    max_attempts     integer NOT NULL DEFAULT 5 CHECK (max_attempts >= 1),
    -- Set while running; every write by a worker is fenced on it.
    lease_token      uuid,
    lease_expires_at timestamptz,
    locked_by        text,
    unique_key       text,
    last_error       text,
    created_at       timestamptz NOT NULL DEFAULT now()
);

-- The claim query's index: lowest priority number first, then oldest.
CREATE INDEX jobs_ready_idx ON jobs (priority, run_at) WHERE state = 'ready';
-- A dead job releases its key, so the same work can be enqueued again.
CREATE UNIQUE INDEX jobs_unique_key_idx ON jobs (unique_key)
    WHERE unique_key IS NOT NULL AND state IN ('ready', 'running');
-- The reaper finds expired leases here.
CREATE INDEX jobs_lease_idx ON jobs (lease_expires_at) WHERE state = 'running';
-- The per-workspace running cap counts here.
CREATE INDEX jobs_workspace_running_idx ON jobs (workspace_id) WHERE state = 'running';

-- Rows churn constantly (insert, claim, delete), so vacuum early and often.
ALTER TABLE jobs SET (
    autovacuum_vacuum_scale_factor = 0.01,
    autovacuum_vacuum_threshold = 100,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 2
);

CREATE TABLE schedules (
    id           uuid PRIMARY KEY,
    workspace_id uuid REFERENCES workspaces (id) ON DELETE CASCADE,
    kind         text NOT NULL,
    cron         text NOT NULL,
    timezone     text NOT NULL DEFAULT 'UTC',
    -- The next slot to fire. Advancing it is the durable record that a slot fired.
    next_run_at  timestamptz NOT NULL,
    payload      jsonb NOT NULL DEFAULT '{}'::jsonb,
    enabled      boolean NOT NULL DEFAULT true
);

CREATE INDEX schedules_due_idx ON schedules (next_run_at) WHERE enabled;
