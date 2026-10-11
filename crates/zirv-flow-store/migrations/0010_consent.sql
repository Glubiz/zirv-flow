-- Consent, topics and suppressions (architecture sections 6, 10.1 and 16.3).
-- Person-scoped columns are named person_id so the erasure scan finds them. Suppressions are
-- keyed by an HMAC of the identifier, never the value, and are not person-scoped: they
-- outlive the person.

CREATE TABLE topics (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    key          text NOT NULL CHECK (key ~ '^[a-z0-9_-]{1,64}$'),
    name         text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    description  text NOT NULL DEFAULT '' CHECK (char_length(description) <= 500),
    channel      text NOT NULL CHECK (channel IN ('email')),
    -- opt_in: allowed only when subscribed. opt_out: allowed unless unsubscribed.
    policy       text NOT NULL CHECK (policy IN ('opt_in', 'opt_out')),
    archived_at  timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, key)
);

-- The append-only history of consent decisions. Only person_id may change, because a merge
-- moves the history to the surviving person; DELETE stays allowed for erasure.
CREATE TABLE consent_changes (
    id            uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id     uuid NOT NULL REFERENCES people (id),
    channel       text NOT NULL CHECK (channel IN ('email')),
    -- Null means the whole channel.
    topic_id      uuid REFERENCES topics (id),
    -- Null on the first decision for the channel or topic.
    from_status   text CHECK (from_status IN
        ('subscribed', 'unsubscribed', 'pending_confirmation', 'unknown')),
    to_status     text NOT NULL CHECK (to_status IN
        ('subscribed', 'unsubscribed', 'pending_confirmation', 'unknown')),
    source        text NOT NULL CHECK (source IN
        ('api', 'ui', 'import', 'merge', 'unsubscribe_link', 'ingest')),
    -- Free keys such as ip, user_agent, form or text; at most 4 KiB.
    evidence      jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(evidence) = 'object' AND octet_length(evidence::text) <= 4096),
    occurred_at   timestamptz NOT NULL,
    actor_user_id uuid,
    trace_id      uuid NOT NULL,
    recorded_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX consent_changes_person_idx
    ON consent_changes (workspace_id, person_id, occurred_at DESC, id DESC);

CREATE FUNCTION consent_changes_append_only() RETURNS trigger AS $$
BEGIN
    IF ROW(NEW.id, NEW.workspace_id, NEW.channel, NEW.topic_id, NEW.from_status,
           NEW.to_status, NEW.source, NEW.evidence, NEW.occurred_at, NEW.actor_user_id,
           NEW.trace_id, NEW.recorded_at)
       IS DISTINCT FROM
       ROW(OLD.id, OLD.workspace_id, OLD.channel, OLD.topic_id, OLD.from_status,
           OLD.to_status, OLD.source, OLD.evidence, OLD.occurred_at, OLD.actor_user_id,
           OLD.trace_id, OLD.recorded_at)
    THEN
        RAISE EXCEPTION 'consent_changes is append-only';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER consent_changes_no_update
    BEFORE UPDATE ON consent_changes
    FOR EACH ROW EXECUTE FUNCTION consent_changes_append_only();

-- The current state per person, channel and topic. A missing row means unknown.
CREATE TABLE consent (
    workspace_id   uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id      uuid NOT NULL REFERENCES people (id),
    channel        text NOT NULL CHECK (channel IN ('email')),
    topic_id       uuid REFERENCES topics (id),
    status         text NOT NULL CHECK (status IN
        ('subscribed', 'unsubscribed', 'pending_confirmation', 'unknown')),
    -- Whether the person decided: only subscribed and unsubscribed are explicit.
    explicit       boolean NOT NULL,
    changed_at     timestamptz NOT NULL,
    last_change_id uuid REFERENCES consent_changes (id) ON DELETE SET NULL,
    UNIQUE NULLS NOT DISTINCT (workspace_id, person_id, channel, topic_id)
);

CREATE TABLE suppressions (
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- HMAC-SHA256 of "<kind>:<normalized value>"; the value itself is never stored.
    identifier_hash bytea NOT NULL CHECK (octet_length(identifier_hash) = 32),
    kind            text NOT NULL CHECK (kind IN ('email', 'phone')),
    reason          text NOT NULL CHECK (reason IN
        ('hard_bounce', 'complaint', 'manual', 'erased')),
    source          text NOT NULL,
    -- Must not hold the address.
    note            text CHECK (char_length(note) <= 500),
    created_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid,
    PRIMARY KEY (workspace_id, identifier_hash)
);

CREATE INDEX suppressions_listing_idx
    ON suppressions (workspace_id, created_at DESC, identifier_hash DESC);
