-- Flows with immutable versions (architecture sections 9.1, 14 and 16.2). A flow holds its draft
-- graph and settings and its lifecycle state; every publish copies the draft into a new
-- immutable version. The runtime (enrollments, steps) has its own tables and is not here.

-- The workspace's IANA time zone: the local clock of `wait.until_time` for a person with no zone
-- of their own. The API checks the name against the zone database; the length bound is a guard.
ALTER TABLE workspaces
    ADD COLUMN timezone text NOT NULL DEFAULT 'UTC' CHECK (char_length(timezone) BETWEEN 1 AND 64);

-- A webhook action signs with a per-workspace secret (architecture 16.2). The purpose is added
-- here so a publish can ask whether the workspace has one; creating and rotating it is the
-- webhook action's own endpoint.
ALTER TABLE encrypted_secrets DROP CONSTRAINT encrypted_secrets_purpose_check;
ALTER TABLE encrypted_secrets ADD CONSTRAINT encrypted_secrets_purpose_check
    CHECK (purpose IN ('connector_credential', 'oauth_token', 'ai_provider_key', 'webhook_secret',
                       'webhook_signing'));

CREATE TABLE flows (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name            text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    -- draft: never published; published: live; paused: no new enrollments; archived: final.
    state           text NOT NULL DEFAULT 'draft'
                    CHECK (state IN ('draft', 'published', 'paused', 'archived')),
    -- The editable copy. It may be incomplete or invalid; only a publish validates it.
    draft_graph     jsonb NOT NULL,
    draft_settings  jsonb NOT NULL,
    -- The newest published version; null until the first publish.
    current_version integer CHECK (current_version > 0),
    archived_at     timestamptz,
    created_by      uuid REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- A flow that was never published is a draft (or was archived as one); a live or paused one
    -- has a version.
    CHECK (state <> 'draft' OR current_version IS NULL),
    CHECK (state NOT IN ('published', 'paused') OR current_version IS NOT NULL),
    CHECK ((state = 'archived') = (archived_at IS NOT NULL))
);

-- A live flow's name is unique in its workspace, ignoring case; an archived name is free again.
CREATE UNIQUE INDEX flows_name_idx ON flows (workspace_id, lower(name)) WHERE state <> 'archived';

CREATE TABLE flow_versions (
    workspace_id uuid NOT NULL,
    flow_id      uuid NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    graph        jsonb NOT NULL,
    settings     jsonb NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    published_by uuid REFERENCES users (id),
    -- The trace of the request that published the version.
    trace_id     uuid NOT NULL,
    PRIMARY KEY (flow_id, version),
    FOREIGN KEY (workspace_id, flow_id) REFERENCES flows (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX flow_versions_workspace_idx ON flow_versions (workspace_id, flow_id);

-- The current version always exists. It is checked at commit, so a publish may insert the version
-- and move the pointer in either order.
ALTER TABLE flows ADD CONSTRAINT flows_current_version_fkey
    FOREIGN KEY (id, current_version) REFERENCES flow_versions (flow_id, version)
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION flow_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'flow versions are immutable';
END
$$;

CREATE TRIGGER flow_versions_immutable
    BEFORE UPDATE ON flow_versions
    FOR EACH ROW EXECUTE FUNCTION flow_versions_immutable();

-- A version is not deleted on its own. Deleting its flow (or the workspace) still removes it:
-- the cascade deletes the parent row first, so this trigger no longer finds it.
CREATE FUNCTION flow_versions_no_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM flows WHERE id = OLD.flow_id) THEN
        RAISE EXCEPTION 'flow versions cannot be deleted while their flow exists';
    END IF;
    RETURN OLD;
END
$$;

CREATE TRIGGER flow_versions_no_delete
    BEFORE DELETE ON flow_versions
    FOR EACH ROW EXECUTE FUNCTION flow_versions_no_delete();
