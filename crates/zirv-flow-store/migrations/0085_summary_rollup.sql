-- The Home figures (`GET /workspaces/{ws}/summary`) are read from a daily rollup, not counted
-- per request. A scheduled job (`summary.rollup`, every five minutes) adds the rows written since
-- its last run to their UTC day and, once a day has closed, recounts that day exactly.

-- One count per workspace, UTC day and metric. `flow_id` is the nil uuid for the workspace-wide
-- metrics (`messages_sent`, `goals_reached`, `people_created`, `events_received`) and a flow for
-- the per-flow ones (`flow_sent`, `flow_goals`). No foreign key on `flow_id`: the nil uuid is
-- not a flow, and the endpoint reads only the flows of `summary_flows`.
CREATE TABLE summary_daily (
    workspace_id uuid   NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    day          date   NOT NULL,
    metric       text   NOT NULL CHECK (metric IN
        ('messages_sent', 'goals_reached', 'people_created', 'events_received',
         'flow_sent', 'flow_goals')),
    flow_id      uuid   NOT NULL,
    count        bigint NOT NULL CHECK (count >= 0),
    PRIMARY KEY (workspace_id, day, metric, flow_id)
);

-- What the job has counted. `counted_to` is the high-water mark: every source row stamped before
-- it is in `summary_daily` (the next run counts `[counted_to, now - 2 minutes)`; the margin lets
-- transactions that started earlier commit). `closed_through` is the last day recounted
-- exactly. `computed_at` is when the run finished, shown as the figures' age. The row also
-- serialises two runs on one workspace.
CREATE TABLE summary_state (
    workspace_id   uuid        PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    counted_to     timestamptz,
    closed_through date,
    computed_at    timestamptz
);

-- A current snapshot per published flow: enrollments in flight, and whether its current version
-- has a goal (so the endpoint tells "no goal" from "no conversions").
CREATE TABLE summary_flows (
    workspace_id   uuid   NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    flow_id        uuid   NOT NULL,
    people_in_flow bigint NOT NULL CHECK (people_in_flow >= 0),
    has_goal       boolean NOT NULL,
    PRIMARY KEY (workspace_id, flow_id)
);

-- Indexes the job's counts need. Events are counted by id range on the existing
-- `events_workspace_id_id_idx` (0069): ids are minted on receipt, so an id range is a received
-- time range, and `events` gets no new index. The other three sources have no index that leads
-- with the workspace and the time column.
--
-- Locking: each statement takes a SHARE lock on its table until the index is built, blocking
-- writes. A populated deployment builds them first, with the same name and definition, and
-- these statements then find them (IF NOT EXISTS):
--     CREATE INDEX CONCURRENTLY people_workspace_created_idx
--         ON people (workspace_id, created_at);
--     CREATE INDEX CONCURRENTLY messages_sent_completed_idx
--         ON messages (workspace_id, completed_at) WHERE status = 'sent' AND NOT test;
--     CREATE INDEX CONCURRENTLY step_executions_workspace_started_idx
--         ON step_executions (workspace_id, started_at);
-- A failed CONCURRENTLY build leaves an invalid index of that name: drop it and build again.

-- New people in a time slice.
CREATE INDEX IF NOT EXISTS people_workspace_created_idx ON people (workspace_id, created_at);

-- Messages sent in a slice: delivered, non-test rows only, so the index stays small next to the
-- skipped and failed ones.
CREATE INDEX IF NOT EXISTS messages_sent_completed_idx ON messages (workspace_id, completed_at)
    WHERE status = 'sent' AND NOT test;

-- Goal rows in a slice: `step_executions_workspace_idx` leads with the node and has no time
-- column, and the job narrows by time first, then by the goal node ids.
CREATE INDEX IF NOT EXISTS step_executions_workspace_started_idx
    ON step_executions (workspace_id, started_at);

-- The rollup job, as an instance-level schedule: every five minutes, first at the next five-minute mark.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled)
SELECT gen_random_uuid(), NULL, 'summary.rollup', '*/5 * * * *', 'UTC',
       date_trunc('hour', now()) + (floor(extract(minute from now()) / 5) + 1) * interval '5 minutes',
       '{}'::jsonb, true
WHERE NOT EXISTS (
    SELECT 1 FROM schedules WHERE kind = 'summary.rollup' AND workspace_id IS NULL
);
