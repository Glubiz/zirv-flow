-- Versioned themes and the workspace default theme (architecture sections 11.3 and 11.7). A theme
-- is a set of design tokens kept apart from any template; its one editable draft is the tokens
-- being worked on, and every publish copies the draft into a new immutable version, as a
-- template's does (0043). The store keeps the tokens as opaque JSON: the templates crate owns
-- their shape.
--
-- None of these tables is person-scoped (a theme holds no person, address or trait value), so
-- the erasure scan and the identity merge have nothing to do here.

CREATE TABLE themes (
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

-- A live theme's name is unique in its workspace, ignoring case; an archived name is free again.
CREATE UNIQUE INDEX themes_name_idx ON themes (workspace_id, lower(name))
    WHERE archived_at IS NULL;

CREATE TABLE theme_drafts (
    workspace_id uuid NOT NULL,
    theme_id     uuid NOT NULL,
    -- The editable tokens. Their shape is checked on save; a publish validates them fully.
    tokens       jsonb NOT NULL,
    -- How many times the draft has been saved; the first save is 1.
    revision     integer NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_by   uuid REFERENCES users (id),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (theme_id),
    FOREIGN KEY (workspace_id, theme_id) REFERENCES themes (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX theme_drafts_workspace_idx ON theme_drafts (workspace_id, theme_id);

CREATE TABLE theme_versions (
    workspace_id uuid NOT NULL,
    theme_id     uuid NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    tokens       jsonb NOT NULL,
    -- The validation issues found at publish time: warnings only, because a blocking issue
    -- refuses the publish and writes no version.
    validation   jsonb NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    published_by uuid REFERENCES users (id),
    -- The trace of the request that published the version.
    trace_id     uuid NOT NULL,
    PRIMARY KEY (theme_id, version),
    FOREIGN KEY (workspace_id, theme_id) REFERENCES themes (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX theme_versions_workspace_idx ON theme_versions (workspace_id, theme_id);

-- The current version always exists. It is checked at commit, so a publish may insert the version
-- and move the pointer in either order.
ALTER TABLE themes ADD CONSTRAINT themes_current_version_fkey
    FOREIGN KEY (id, current_version) REFERENCES theme_versions (theme_id, version)
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION theme_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'theme versions are immutable';
END
$$;

CREATE TRIGGER theme_versions_immutable
    BEFORE UPDATE ON theme_versions
    FOR EACH ROW EXECUTE FUNCTION theme_versions_immutable();

-- A version is not deleted on its own. Deleting its theme (or the workspace) still removes it:
-- the cascade deletes the parent row first, so this trigger no longer finds it.
CREATE FUNCTION theme_versions_no_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM themes WHERE id = OLD.theme_id) THEN
        RAISE EXCEPTION 'theme versions cannot be deleted while their theme exists';
    END IF;
    RETURN OLD;
END
$$;

CREATE TRIGGER theme_versions_no_delete
    BEFORE DELETE ON theme_versions
    FOR EACH ROW EXECUTE FUNCTION theme_versions_no_delete();

-- The theme a template follows when its document says `theme: "default"`. The pair of columns
-- references the theme, so the default can only be a theme of the same workspace; that it is
-- published and not archived is checked, under the theme's row lock, when it is set.
CREATE TABLE workspace_default_theme (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    theme_id     uuid NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, theme_id) REFERENCES themes (workspace_id, id) ON DELETE CASCADE
);
