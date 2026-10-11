-- The profile store (architecture sections 6 and 7): people, their identities, the ledger of
-- what each source said, the field registry, identity conflicts and the dirty set.
-- Every table is workspace-scoped and every index leads with workspace_id. Person-scoped
-- columns are named person_id or <role>_person_id so the erasure scan can find them.

CREATE TABLE people (
    id            uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Materialized values by key, and the name of the source that supplied each.
    traits        jsonb NOT NULL DEFAULT '{}'::jsonb,
    trait_sources jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Unused until merges arrive.
    merged_into   uuid REFERENCES people (id),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX people_workspace_idx ON people (workspace_id, id);

CREATE TABLE identities (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id    uuid NOT NULL REFERENCES people (id),
    kind         text NOT NULL,
    value        text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, kind, value)
);

CREATE INDEX identities_person_idx ON identities (workspace_id, person_id);

-- The ledger: what each source record said, kept per record so a trait can be recomputed
-- when one source retracts it.
CREATE TABLE source_records (
    id            uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    source        text NOT NULL,
    stream        text NOT NULL,
    record_key    text NOT NULL,
    person_id     uuid NOT NULL REFERENCES people (id),
    -- Normalized [{kind, value}].
    identifiers   jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Key to {"v": value, "at": rfc3339}: each key carries its own observation time.
    traits        jsonb NOT NULL DEFAULT '{}'::jsonb,
    content_hash  bytea NOT NULL,
    last_seen_run uuid,
    deleted_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, source, stream, record_key)
);

CREATE INDEX source_records_person_idx ON source_records (workspace_id, person_id);

CREATE TABLE trait_definitions (
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    key             text NOT NULL,
    type            text NOT NULL
        CHECK (type IN ('string', 'number', 'boolean', 'timestamp', 'date', 'string_list')),
    precedence      text NOT NULL DEFAULT 'latest'
        CHECK (precedence IN ('latest', 'source_priority', 'first')),
    source_order    text[] NOT NULL DEFAULT '{}',
    pii             boolean NOT NULL DEFAULT false,
    channel_address boolean NOT NULL DEFAULT false,
    inferred        boolean NOT NULL DEFAULT false,
    archived        boolean NOT NULL DEFAULT false,
    description     text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, key)
);

CREATE TABLE identity_conflicts (
    id               uuid PRIMARY KEY,
    workspace_id     uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The person the record attached to, and the other person its identifier belongs to.
    person_id        uuid NOT NULL REFERENCES people (id),
    other_person_id  uuid NOT NULL REFERENCES people (id),
    kind             text NOT NULL,
    source_record_id uuid NOT NULL REFERENCES source_records (id),
    reason           text NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    resolved_at      timestamptz
);

CREATE INDEX identity_conflicts_person_idx ON identity_conflicts (workspace_id, person_id);

-- The coalesced set of people whose traits changed: one row per person keeps it bounded.
CREATE TABLE person_changes (
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id    uuid NOT NULL REFERENCES people (id),
    changed_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, person_id)
);
