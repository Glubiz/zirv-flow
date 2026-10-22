-- Consent dimensions (architecture sections 6 and 10.1, OQ-32). A workspace declares dimensions
-- such as a vertical or a brand, each with enumerated value keys. A consent record holds one
-- value per dimension it is scoped to; an omitted dimension means all values.
--
-- The channel stays built in, and the `topic` dimension is built in too: its values are the
-- existing `topics` rows, resolved by the store, so nothing is copied and no existing consent
-- row changes meaning. A record's topic stays in `topic_id`.
--
-- Neither dimension table is person-scoped, so the erasure scan has nothing to find in them.
-- The new `dimensions` columns sit on `consent` and `consent_changes`, which erasure already
-- clears by `person_id`.

CREATE TABLE consent_dimensions (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- `topic` is the built-in dimension and cannot be declared.
    key          text NOT NULL CHECK (key ~ '^[a-z0-9_-]{1,64}$' AND key <> 'topic'),
    name         text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    archived_at  timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, key),
    UNIQUE (workspace_id, id)
);

CREATE TABLE consent_dimension_values (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    dimension_id uuid NOT NULL,
    key          text NOT NULL CHECK (key ~ '^[a-z0-9_-]{1,64}$'),
    name         text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    archived_at  timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (dimension_id, key),
    FOREIGN KEY (workspace_id, dimension_id)
        REFERENCES consent_dimensions (workspace_id, id) ON DELETE CASCADE
);

-- The canonical text of a dimension map: `key=value` pairs sorted bytewise by key and joined by
-- `;`, empty for the empty map. Keys and values are `[a-z0-9_-]` only (the store validates them
-- against the catalog), so the text is unambiguous and equal maps give equal text. The Rust
-- `canonical_dimensions` produces the same text.
CREATE FUNCTION consent_dimensions_canonical(map jsonb) RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS
$$
    SELECT COALESCE(string_agg(e.key || '=' || e.value, ';' ORDER BY e.key COLLATE "C"), '')
    FROM jsonb_each_text(map) AS e
$$;

ALTER TABLE consent_changes
    ADD COLUMN dimensions jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(dimensions) = 'object');

ALTER TABLE consent
    ADD COLUMN dimensions jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(dimensions) = 'object'),
    ADD COLUMN dimensions_key text GENERATED ALWAYS AS (consent_dimensions_canonical(dimensions))
        STORED;

-- The key gains the normalised map. NULLS NOT DISTINCT stays, so a channel-wide record (null
-- topic) is still unique per person, channel and map.
ALTER TABLE consent DROP CONSTRAINT consent_workspace_id_person_id_channel_topic_id_key;
ALTER TABLE consent ADD CONSTRAINT consent_scope_key
    UNIQUE NULLS NOT DISTINCT (workspace_id, person_id, channel, topic_id, dimensions_key);

-- The append-only guard now covers the dimension map too.
CREATE OR REPLACE FUNCTION consent_changes_append_only() RETURNS trigger AS $$
BEGIN
    IF ROW(NEW.id, NEW.workspace_id, NEW.channel, NEW.topic_id, NEW.dimensions, NEW.from_status,
           NEW.to_status, NEW.source, NEW.evidence, NEW.occurred_at, NEW.actor_user_id,
           NEW.trace_id, NEW.recorded_at)
       IS DISTINCT FROM
       ROW(OLD.id, OLD.workspace_id, OLD.channel, OLD.topic_id, OLD.dimensions, OLD.from_status,
           OLD.to_status, OLD.source, OLD.evidence, OLD.occurred_at, OLD.actor_user_id,
           OLD.trace_id, OLD.recorded_at)
    THEN
        RAISE EXCEPTION 'consent_changes is append-only';
    END IF;
    IF NEW.person_id IS DISTINCT FROM OLD.person_id
       AND NEW.person_id IS DISTINCT FROM
           (SELECT merged_into FROM people WHERE id = OLD.person_id)
    THEN
        RAISE EXCEPTION 'consent_changes can only move to the survivor of a merge';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
