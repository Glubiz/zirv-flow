-- The flow runtime (architecture sections 9.2 and 9.3): enrollments pinned to a flow version,
-- the step executions that make a step idempotent, the event waits apply-batch checks, and the
-- entries that re-entry rules read. No cause kind is added: an enrollment carries the cause of
-- the trigger that enrolled it, and the jobs and decisions of its steps carry the same one.

CREATE TABLE enrollments (
    id            uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    flow_id       uuid NOT NULL,
    -- The version the person entered by. An enrollment never migrates (ruling 469cfc51).
    flow_version  integer NOT NULL CHECK (flow_version > 0),
    -- A person's erasure deletes their enrollments with them (architecture 16.3).
    person_id     uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    -- A yield at a branch is transient inside one transaction and is never stored.
    state         text NOT NULL
                  CHECK (state IN ('active', 'waiting', 'completed', 'exited', 'failed')),
    -- The node to run next, or the node the enrollment stopped at.
    node          text NOT NULL CHECK (char_length(node) BETWEEN 1 AND 200),
    -- Ids only (ruling 5c679ecb and architecture 9.3): the trigger kind and its occurrence id.
    context       jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- The trigger occurrence, for idempotent enrolling; also in `context`.
    occurrence_id uuid NOT NULL,
    -- When the timer of a wait fires (the timeout, for `wait.until_event`).
    wake_at       timestamptz,
    -- Grows by one with every committed step; a job carries the one it expects.
    state_version bigint NOT NULL DEFAULT 0 CHECK (state_version >= 0),
    -- Why the enrollment ended other than by reaching an exit node, and the error of a failure.
    exit_reason   text,
    error         text,
    trace_id      uuid NOT NULL,
    cause_kind    text NOT NULL
                  CHECK (cause_kind IN ('event', 'trait_change', 'schedule', 'manual', 'api',
                                        'definition_change', 'sync_run')),
    cause_id      text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    ended_at      timestamptz,
    FOREIGN KEY (workspace_id, flow_id) REFERENCES flows (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (flow_id, flow_version) REFERENCES flow_versions (flow_id, version)
        ON DELETE CASCADE,
    CHECK ((state IN ('completed', 'exited')) = (ended_at IS NOT NULL)),
    CHECK (state <> 'failed' OR error IS NOT NULL)
);

-- One in-flight enrollment per flow and person. A failed one still holds the person: an operator
-- retries or skips it, and that moves it back to active.
CREATE UNIQUE INDEX enrollments_in_flight_idx ON enrollments (flow_id, person_id)
    WHERE state IN ('active', 'waiting', 'failed');
-- A redelivered fan-out job enrolls nobody twice.
CREATE UNIQUE INDEX enrollments_occurrence_idx ON enrollments (flow_id, person_id, occurrence_id);
CREATE INDEX enrollments_person_idx ON enrollments (workspace_id, person_id);
-- The publish dry run counts the in-flight enrollments of a flow.
CREATE INDEX enrollments_flow_state_idx ON enrollments (flow_id, state, flow_version);

-- A node an enrollment has run. The row is the step's idempotency record: its effects are applied
-- only by the transaction that inserts it (architecture 9.3).
CREATE TABLE step_executions (
    id            uuid NOT NULL UNIQUE,
    workspace_id  uuid NOT NULL,
    enrollment_id uuid NOT NULL REFERENCES enrollments (id) ON DELETE CASCADE,
    node_id       text NOT NULL CHECK (char_length(node_id) BETWEEN 1 AND 200),
    outcome       text NOT NULL CHECK (outcome IN ('done', 'waiting', 'failed')),
    error         text,
    -- The attempt of the job that ran the step.
    attempts      integer NOT NULL DEFAULT 1 CHECK (attempts >= 1),
    started_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (enrollment_id, node_id),
    CHECK ((outcome = 'failed') = (error IS NOT NULL))
);

-- The per-node counts of a flow read this, bounded by a statement timeout.
CREATE INDEX step_executions_workspace_idx ON step_executions (workspace_id, node_id, outcome);

-- An enrollment asleep at a `wait.until_event`. Apply-batch matches inserted events against it.
CREATE TABLE enrollment_waits (
    workspace_id  uuid NOT NULL,
    enrollment_id uuid NOT NULL REFERENCES enrollments (id) ON DELETE CASCADE,
    person_id     uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    event_name    text NOT NULL,
    -- The property filters, in the segment AST's wire form; the step job applies them.
    filters       jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- The `state_version` the wait was registered at, which the matching job expects.
    wake_version  bigint NOT NULL,
    timeout_at    timestamptz NOT NULL,
    PRIMARY KEY (enrollment_id, event_name)
);

CREATE INDEX enrollment_waits_match_idx ON enrollment_waits (workspace_id, person_id, event_name);

-- How often a person entered a flow and when last: the `once` and `always` re-entry rules.
CREATE TABLE flow_entries (
    workspace_id    uuid NOT NULL,
    flow_id         uuid NOT NULL,
    person_id       uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    entries         integer NOT NULL CHECK (entries >= 1),
    last_entered_at timestamptz NOT NULL,
    PRIMARY KEY (flow_id, person_id),
    FOREIGN KEY (workspace_id, flow_id) REFERENCES flows (workspace_id, id) ON DELETE CASCADE
);
