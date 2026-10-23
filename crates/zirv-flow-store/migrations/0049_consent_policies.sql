-- Consent policies (architecture sections 6 and 10.1, OQ-32). A workspace has at most one
-- policy: an ordered list of named deny-only rules over the segment condition AST, with the
-- built-in `recorded_consent` rule always present. The policy has a draft that is edited and
-- immutable published versions; the send gate evaluates the published one. A workspace with no
-- row, or with no published version, behaves as if only the built-in rule existed.
--
-- Neither table is person-scoped, so the erasure scan has nothing to find in them. The rule
-- that decided a send is kept on `messages.deciding_rule`, which erasure already clears.

CREATE TABLE consent_policies (
    id                uuid PRIMARY KEY,
    workspace_id      uuid NOT NULL UNIQUE REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The rules being edited: [{"id", "name", "condition"?}], in order. The built-in rule has no
    -- condition. The API checks the shape and the conditions; the store keeps what it is given.
    draft_rules       jsonb NOT NULL CHECK (jsonb_typeof(draft_rules) = 'array'),
    -- Counts the saves of the draft, so a publish can name the draft it validated.
    draft_revision    integer NOT NULL DEFAULT 1 CHECK (draft_revision > 0),
    -- The version the send gate reads; none before the first publish.
    published_version integer CHECK (published_version > 0),
    updated_by        uuid REFERENCES users (id),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

CREATE TABLE consent_policy_versions (
    workspace_id uuid NOT NULL,
    policy_id    uuid NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    rules        jsonb NOT NULL CHECK (jsonb_typeof(rules) = 'array'),
    -- The trace of the request that published the version; the definition change is attributed
    -- to it.
    trace_id     uuid NOT NULL,
    published_by uuid REFERENCES users (id),
    published_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (policy_id, version),
    FOREIGN KEY (workspace_id, policy_id)
        REFERENCES consent_policies (workspace_id, id) ON DELETE CASCADE
);

ALTER TABLE consent_policies ADD CONSTRAINT consent_policies_published_version_fkey
    FOREIGN KEY (id, published_version) REFERENCES consent_policy_versions (policy_id, version);

CREATE FUNCTION consent_policy_versions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'consent policy versions are immutable';
END
$$;

CREATE TRIGGER consent_policy_versions_immutable
    BEFORE UPDATE ON consent_policy_versions
    FOR EACH ROW EXECUTE FUNCTION consent_policy_versions_immutable();
