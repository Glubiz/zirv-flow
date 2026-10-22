-- Message templates with immutable published versions (architecture sections 11.6, 11.7 and 15).
-- A template holds its settings; its one editable draft is the content document being worked on,
-- and every publish copies the draft into a new immutable version. The store keeps the document
-- as opaque JSON: the content-type registry in the templates crate owns its shape, so a new
-- content type changes no table.
--
-- None of these tables is person-scoped (a template holds no person, address or trait value), so
-- the erasure scan has nothing to find here. Saved preview people are not stored (M7).

CREATE TABLE templates (
    id                 uuid PRIMARY KEY,
    workspace_id       uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name               text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    channel            text NOT NULL CHECK (channel IN ('email', 'webhook')),
    category           text NOT NULL CHECK (category IN ('marketing', 'transactional')),
    -- The sender identity of an email template. No foreign key yet: the identities table and the
    -- verified-domain check arrive with the sender-domain work.
    sender_identity_id uuid,
    -- The newest published version; null until the first publish.
    current_version    integer CHECK (current_version > 0),
    archived_at        timestamptz,
    created_by         uuid REFERENCES users (id),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

-- A live template's name is unique in its workspace, ignoring case; an archived name is free again.
CREATE UNIQUE INDEX templates_name_idx ON templates (workspace_id, lower(name))
    WHERE archived_at IS NULL;

CREATE TABLE template_drafts (
    workspace_id uuid NOT NULL,
    template_id  uuid NOT NULL,
    -- The editable content document. It may be incomplete or invalid; only a publish validates it.
    document     jsonb NOT NULL,
    -- How many times the draft has been saved; the first save is 1.
    revision     integer NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by   uuid REFERENCES users (id),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (template_id),
    FOREIGN KEY (workspace_id, template_id) REFERENCES templates (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX template_drafts_workspace_idx ON template_drafts (workspace_id, template_id);

CREATE TABLE template_versions (
    workspace_id uuid NOT NULL,
    template_id  uuid NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    document     jsonb NOT NULL,
    -- The validation issues found at publish time: warnings only, because a blocking issue
    -- refuses the publish and writes no version.
    validation   jsonb NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    published_by uuid REFERENCES users (id),
    -- The trace of the request that published the version.
    trace_id     uuid NOT NULL,
    PRIMARY KEY (template_id, version),
    FOREIGN KEY (workspace_id, template_id) REFERENCES templates (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX template_versions_workspace_idx ON template_versions (workspace_id, template_id);

-- The current version always exists. It is checked at commit, so a publish may insert the version
-- and move the pointer in either order.
ALTER TABLE templates ADD CONSTRAINT templates_current_version_fkey
    FOREIGN KEY (id, current_version) REFERENCES template_versions (template_id, version)
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION template_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'template versions are immutable';
END
$$;

CREATE TRIGGER template_versions_immutable
    BEFORE UPDATE ON template_versions
    FOR EACH ROW EXECUTE FUNCTION template_versions_immutable();

-- A version is not deleted on its own. Deleting its template (or the workspace) still removes it:
-- the cascade deletes the parent row first, so this trigger no longer finds it.
CREATE FUNCTION template_versions_no_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM templates WHERE id = OLD.template_id) THEN
        RAISE EXCEPTION 'template versions cannot be deleted while their template exists';
    END IF;
    RETURN OLD;
END
$$;

CREATE TRIGGER template_versions_no_delete
    BEFORE DELETE ON template_versions
    FOR EACH ROW EXECUTE FUNCTION template_versions_no_delete();
