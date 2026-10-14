-- Segments with versions (architecture sections 6 and 8). A segment is a named definition; every
-- save of the definition is an immutable version and the newest one is current (an editor saves
-- the current version, there is no separate publish). Both tables are workspace-scoped with
-- composite workspace keys, and neither holds a person: membership arrives with its own tables.

CREATE TABLE segments (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    description     text NOT NULL DEFAULT '' CHECK (char_length(description) <= 500),
    current_version integer NOT NULL CHECK (current_version > 0),
    -- How often a time-sensitive segment is recomputed in full, in minutes, one of a fixed set from 5 minutes to 7 days (ruling 303ac6ac).
    cadence_minutes integer NOT NULL DEFAULT 60 CHECK (cadence_minutes IN (5, 10, 15, 30, 60, 120, 180, 240, 360, 480, 720, 1440, 10080)),
    archived_at     timestamptz,
    created_by      uuid REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

-- A live segment's name is unique in its workspace, ignoring case; an archived name is free again.
CREATE UNIQUE INDEX segments_name_idx ON segments (workspace_id, lower(name))
    WHERE archived_at IS NULL;

CREATE TABLE segment_versions (
    workspace_id  uuid NOT NULL,
    segment_id    uuid NOT NULL,
    version       integer NOT NULL CHECK (version > 0),
    -- The definition as it was saved.
    definition    jsonb NOT NULL,
    -- normalize(definition): what the compiler lowers.
    normalized    jsonb NOT NULL,
    -- The compiler's dependency summary, including the change classes.
    deps          jsonb NOT NULL,
    time_sensitive boolean NOT NULL,
    -- The trace of the request that saved the version; a definition change is attributed to it.
    trace_id      uuid NOT NULL,
    created_by    uuid REFERENCES users (id),
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (segment_id, version),
    FOREIGN KEY (workspace_id, segment_id) REFERENCES segments (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX segment_versions_workspace_idx ON segment_versions (workspace_id, segment_id);

-- The current version always exists. It is checked at commit, because a new segment is inserted
-- before its first version in the same transaction.
ALTER TABLE segments ADD CONSTRAINT segments_current_version_fkey
    FOREIGN KEY (id, current_version) REFERENCES segment_versions (segment_id, version)
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION segment_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'segment versions are immutable';
END
$$;

CREATE TRIGGER segment_versions_immutable
    BEFORE UPDATE ON segment_versions
    FOR EACH ROW EXECUTE FUNCTION segment_versions_immutable();

-- A version is not deleted on its own. Deleting its segment (or the workspace) still removes it:
-- the cascade deletes the parent row first, so this trigger no longer finds it.
CREATE FUNCTION segment_versions_no_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM segments WHERE id = OLD.segment_id) THEN
        RAISE EXCEPTION 'segment versions cannot be deleted while their segment exists';
    END IF;
    RETURN OLD;
END
$$;

CREATE TRIGGER segment_versions_no_delete
    BEFORE DELETE ON segment_versions
    FOR EACH ROW EXECUTE FUNCTION segment_versions_no_delete();
