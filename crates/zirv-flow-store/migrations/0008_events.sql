-- The governed event store (architecture sections 12.1 and 12.2): customer events, their
-- dedupe records, the schema registry with immutable versions, the workspace event policy and
-- ingest rejects. Every table is workspace-scoped; person-scoped columns are named person_id
-- so the erasure scan can find them.

-- Partitioned by month on occurred_at, like decision_records, so retention can drop whole
-- partitions. The DEFAULT partition catches a row whose month has no partition, so an insert
-- never fails.
CREATE TABLE events (
    id                 uuid NOT NULL,
    workspace_id       uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id          uuid NOT NULL REFERENCES people (id),
    name               text NOT NULL,
    schema_version     integer,
    occurred_at        timestamptz NOT NULL,
    received_at        timestamptz NOT NULL,
    -- Redacted before storage.
    properties         jsonb NOT NULL DEFAULT '{}'::jsonb,
    source             text NOT NULL,
    message_id         text,
    trace_id           uuid NOT NULL,
    validation_state   text NOT NULL CHECK (validation_state IN ('valid', 'flagged', 'unplanned')),
    -- [{property, reason}]: names and codes only, never values.
    validation_reasons jsonb NOT NULL DEFAULT '[]'::jsonb,
    flags              text[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (occurred_at, id)
) PARTITION BY RANGE (occurred_at);

CREATE TABLE events_default PARTITION OF events DEFAULT;

CREATE INDEX events_person_idx ON events (workspace_id, person_id, occurred_at DESC);
CREATE INDEX events_name_idx ON events (workspace_id, name, occurred_at DESC);

-- Partitions for the months from the default retention (760 days) back to the next month, so a
-- backfill within retention does not land in DEFAULT. Names and bounds match the leader's
-- upkeep in the store crate, which then finds them existing.
DO $$
DECLARE
    month_start timestamptz;
BEGIN
    FOR month_start IN
        SELECT m AT TIME ZONE 'UTC'
        FROM generate_series(
            date_trunc('month', (now() - interval '760 days') AT TIME ZONE 'UTC'),
            date_trunc('month', now() AT TIME ZONE 'UTC') + interval '1 month',
            interval '1 month'
        ) AS m
    LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS events_y%sm%s PARTITION OF events '
            'FOR VALUES FROM (%L) TO (%L)',
            to_char(month_start AT TIME ZONE 'UTC', 'YYYY'),
            to_char(month_start AT TIME ZONE 'UTC', 'MM'),
            to_char(month_start AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') || '+00',
            to_char(month_start AT TIME ZONE 'UTC' + interval '1 month', 'YYYY-MM-DD HH24:MI:SS') || '+00'
        );
    END LOOP;
END
$$;

-- One row per client messageId that produced a stored event.
CREATE TABLE event_dedupe (
    workspace_id      uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    message_id        text NOT NULL,
    event_id          uuid NOT NULL,
    event_occurred_at timestamptz NOT NULL,
    person_id         uuid NOT NULL REFERENCES people (id),
    duplicates        integer NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, message_id)
);

CREATE INDEX event_dedupe_age_idx ON event_dedupe (event_occurred_at);

CREATE TABLE event_schemas (
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL,
    mode            text NOT NULL DEFAULT 'warn' CHECK (mode IN ('allow', 'warn', 'block')),
    -- The editable property list; cleared when it is published.
    draft           jsonb,
    current_version integer,
    retention_days  integer CHECK (retention_days > 0),
    description     text NOT NULL DEFAULT '',
    -- The draft was inferred from observed events rather than written by a person.
    inferred        boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE event_schema_versions (
    workspace_id uuid NOT NULL,
    name         text NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    properties   jsonb NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    published_by uuid,
    PRIMARY KEY (workspace_id, name, version),
    FOREIGN KEY (workspace_id, name) REFERENCES event_schemas (workspace_id, name) ON DELETE CASCADE
);

CREATE FUNCTION event_schema_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'event schema versions are immutable';
END
$$;

CREATE TRIGGER event_schema_versions_immutable
    BEFORE UPDATE ON event_schema_versions
    FOR EACH ROW EXECUTE FUNCTION event_schema_versions_immutable();

-- A missing row means the defaults.
CREATE TABLE event_policies (
    workspace_id        uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    unplanned           text NOT NULL DEFAULT 'draft' CHECK (unplanned IN ('draft', 'reject')),
    future_skew_seconds integer NOT NULL DEFAULT 300 CHECK (future_skew_seconds >= 0),
    lateness_seconds    integer NOT NULL DEFAULT 86400 CHECK (lateness_seconds >= 0),
    redaction           jsonb NOT NULL
        DEFAULT '{"drop": [], "hash": [], "truncate_ip": ["ip", "context.ip"]}'::jsonb,
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE ingest_rejects (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    received_at     timestamptz NOT NULL,
    source          text NOT NULL,
    kind            text NOT NULL CHECK (kind IN ('event', 'record')),
    name            text,
    reason          text NOT NULL,
    -- [{property, reason}]
    reasons         jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Masked; the only part the API returns.
    sample          jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- The redacted payload, kept for replay; never returned by the API.
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    person_id       uuid REFERENCES people (id),
    message_id      text,
    trace_id        uuid NOT NULL,
    replayed_at     timestamptz,
    replay_event_id uuid
);

CREATE INDEX ingest_rejects_received_idx ON ingest_rejects (workspace_id, received_at DESC);
CREATE INDEX ingest_rejects_person_idx ON ingest_rejects (workspace_id, person_id);
-- A message that was rejected and is still open has one reject, however often it is resent.
CREATE UNIQUE INDEX ingest_rejects_open_message_idx ON ingest_rejects (workspace_id, message_id)
    WHERE message_id IS NOT NULL AND replayed_at IS NULL;

-- The earliest occurred_at of a late event since the mark was last consumed.
ALTER TABLE person_changes ADD COLUMN late_from timestamptz;

-- A conflict found while storing an event has no source record.
ALTER TABLE identity_conflicts ALTER COLUMN source_record_id DROP NOT NULL;

-- One open conflict per person pair and source record, whoever records it and however often.
-- Open duplicates that exist already are closed, keeping the oldest.
UPDATE identity_conflicts c SET resolved_at = now(), resolution = 'kept_separate'
WHERE c.resolved_at IS NULL AND EXISTS (
    SELECT 1 FROM identity_conflicts o
    WHERE o.resolved_at IS NULL AND o.workspace_id = c.workspace_id
      AND o.person_id = c.person_id AND o.other_person_id = c.other_person_id
      AND COALESCE(o.source_record_id, '00000000-0000-0000-0000-000000000000'::uuid)
        = COALESCE(c.source_record_id, '00000000-0000-0000-0000-000000000000'::uuid)
      AND (o.created_at, o.id) < (c.created_at, c.id));
CREATE UNIQUE INDEX identity_conflicts_open_idx ON identity_conflicts
    (workspace_id, person_id, other_person_id,
     (COALESCE(source_record_id, '00000000-0000-0000-0000-000000000000'::uuid)))
    WHERE resolved_at IS NULL;

-- The daily retention job, as an instance-level schedule: 03:17 UTC, first at the next one.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled)
SELECT gen_random_uuid(), NULL, 'events.retention', '17 3 * * *', 'UTC',
       CASE
           WHEN date_trunc('day', now() AT TIME ZONE 'UTC') + interval '3 hours 17 minutes'
                > now() AT TIME ZONE 'UTC'
               THEN (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '3 hours 17 minutes')
                    AT TIME ZONE 'UTC'
           ELSE (date_trunc('day', now() AT TIME ZONE 'UTC') + interval '1 day 3 hours 17 minutes')
                AT TIME ZONE 'UTC'
       END,
       '{}'::jsonb, true
WHERE NOT EXISTS (
    SELECT 1 FROM schedules WHERE kind = 'events.retention' AND workspace_id IS NULL
);
