-- Versioned snippets (architecture sections 11.3 and 11.7). A snippet is a block list kept outside
-- any template; its one editable draft is the content being worked on, and every publish copies
-- the draft into a new immutable version, as a template's does (0043). The store keeps the content
-- as opaque JSON: the templates crate owns its shape.
--
-- None of these tables is person-scoped (a snippet holds no person, address or trait value), so
-- the erasure scan and the identity merge have nothing to do here.

CREATE TABLE snippets (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    -- The newest published version; null until the first publish.
    current_version integer CHECK (current_version > 0),
    archived_at     timestamptz,
    created_by      uuid REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

-- A live snippet's name is unique in its workspace, ignoring case; an archived name is free again.
CREATE UNIQUE INDEX snippets_name_idx ON snippets (workspace_id, lower(name))
    WHERE archived_at IS NULL;

CREATE TABLE snippet_drafts (
    workspace_id uuid NOT NULL,
    snippet_id   uuid NOT NULL,
    -- The editable content. It may be incomplete or invalid; only a publish validates it.
    content      jsonb NOT NULL,
    -- How many times the draft has been saved; the first save is 1.
    revision     integer NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by   uuid REFERENCES users (id),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (snippet_id),
    FOREIGN KEY (workspace_id, snippet_id) REFERENCES snippets (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX snippet_drafts_workspace_idx ON snippet_drafts (workspace_id, snippet_id);

CREATE TABLE snippet_versions (
    workspace_id uuid NOT NULL,
    snippet_id   uuid NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    content      jsonb NOT NULL,
    -- The validation issues found at publish time: warnings only, because a blocking issue
    -- refuses the publish and writes no version.
    validation   jsonb NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    published_by uuid REFERENCES users (id),
    -- The trace of the request that published the version.
    trace_id     uuid NOT NULL,
    PRIMARY KEY (snippet_id, version),
    FOREIGN KEY (workspace_id, snippet_id) REFERENCES snippets (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX snippet_versions_workspace_idx ON snippet_versions (workspace_id, snippet_id);

-- The current version always exists. It is checked at commit, so a publish may insert the version
-- and move the pointer in either order.
ALTER TABLE snippets ADD CONSTRAINT snippets_current_version_fkey
    FOREIGN KEY (id, current_version) REFERENCES snippet_versions (snippet_id, version)
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION snippet_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'snippet versions are immutable';
END
$$;

CREATE TRIGGER snippet_versions_immutable
    BEFORE UPDATE ON snippet_versions
    FOR EACH ROW EXECUTE FUNCTION snippet_versions_immutable();

-- A version is not deleted on its own. Deleting its snippet (or the workspace) still removes it:
-- the cascade deletes the parent row first, so this trigger no longer finds it.
CREATE FUNCTION snippet_versions_no_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM snippets WHERE id = OLD.snippet_id) THEN
        RAISE EXCEPTION 'snippet versions cannot be deleted while their snippet exists';
    END IF;
    RETURN OLD;
END
$$;

CREATE TRIGGER snippet_versions_no_delete
    BEFORE DELETE ON snippet_versions
    FOR EACH ROW EXECUTE FUNCTION snippet_versions_no_delete();
