-- Event forwarding (architecture 12.5 and 14): the rules that pick accepted events for a
-- destination, and the transactional outbox those events are copied into, in the transaction that
-- writes them.
--
-- `forwarding_outbox` is person-scoped: its payload is whatever the rule's mapping took from the
-- event, which may be personal data. `person_id` lets erasure delete the rows and a merge re-point
-- them. `delivery_attempts` already names a forwarding item by id (`subject_kind = 'forwarding'`)
-- and holds no person.

CREATE TABLE forwarding_rules (
    id             uuid PRIMARY KEY,
    workspace_id   uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    destination_id uuid NOT NULL,
    name           text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    -- The exact event name the rule selects.
    event_name     text NOT NULL CHECK (char_length(event_name) BETWEEN 1 AND 256),
    -- Property predicates, all of which must hold: the segment AST `where` list.
    filter         jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Output key to source: evaluated in SQL by the outbox insert.
    mapping        jsonb NOT NULL,
    active         boolean NOT NULL DEFAULT true,
    created_by     uuid REFERENCES users (id),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    FOREIGN KEY (workspace_id, destination_id) REFERENCES destinations (workspace_id, id)
        ON DELETE CASCADE
);

-- The ingest path asks "is any rule active in this workspace" once per batch.
CREATE INDEX forwarding_rules_active_idx ON forwarding_rules (workspace_id) WHERE active;
CREATE UNIQUE INDEX forwarding_rules_name_idx ON forwarding_rules (workspace_id, lower(name));
CREATE INDEX forwarding_rules_destination_idx ON forwarding_rules (workspace_id, destination_id);

CREATE TABLE forwarding_outbox (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    destination_id  uuid NOT NULL,
    -- The rule that selected the event. Informational: deleting the rule keeps its rows.
    rule_id         uuid NOT NULL,
    event_id        uuid NOT NULL,
    person_id       uuid NOT NULL REFERENCES people (id),
    -- A stable hash of the event id and the destination id; every retry sends it.
    idempotency_key text NOT NULL,
    -- The mapped payload. Emptied once delivered.
    payload         jsonb NOT NULL,
    state           text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'delivered', 'dead')),
    -- Attempts of the current cycle; a re-drive resets it and adds it to `attempt_offset`, so
    -- the numbers recorded in `delivery_attempts` never repeat.
    attempts        integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    attempt_offset  integer NOT NULL DEFAULT 0 CHECK (attempt_offset >= 0),
    -- A fixed code (a verdict reason), never a message, a URL or a value.
    last_error      text CHECK (last_error ~ '^[a-z0-9_]{1,64}$'),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (destination_id, event_id),
    FOREIGN KEY (workspace_id, destination_id) REFERENCES destinations (workspace_id, id)
        ON DELETE CASCADE
);

-- The due-row claim, the depth and the lag of the status read and the gauges.
CREATE INDEX forwarding_outbox_due_idx ON forwarding_outbox (destination_id, next_attempt_at)
    WHERE state = 'pending';
-- The dead-letter list, newest first, and the bulk re-drive.
CREATE INDEX forwarding_outbox_dead_idx ON forwarding_outbox (workspace_id, destination_id,
    updated_at DESC, id DESC) WHERE state = 'dead';
-- Delivered rows are kept a few days for the idempotency guard, then purged by age.
CREATE INDEX forwarding_outbox_delivered_idx ON forwarding_outbox (updated_at)
    WHERE state = 'delivered';
-- Erasure and the merge rewrite find a person's rows here.
CREATE INDEX forwarding_outbox_person_idx ON forwarding_outbox (workspace_id, person_id);

-- At most one `event.forward` job is ready per destination: it is found by the destination in its
-- payload, as the send queue's gauges find theirs.
CREATE INDEX jobs_event_forward_idx ON jobs ((payload ->> 'destination_id'), state)
    WHERE kind = 'event.forward' AND state IN ('ready', 'running');
