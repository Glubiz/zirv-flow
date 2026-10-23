-- Workspace credentials (architecture 10.1 and 16.2): a named, typed credential that several
-- sources and destinations share, rotate in one place and test. The non-secret values are
-- `fields`; every secret value is one sealed JSON object in `encrypted_secrets` (`secret_id`).
-- Nothing here is person-scoped: no column holds a person, an address or a secret value.

-- A workspace credential's secret has its own purpose. Every earlier purpose is listed again:
-- the CHECK is replaced, not extended.
ALTER TABLE encrypted_secrets DROP CONSTRAINT encrypted_secrets_purpose_check;
ALTER TABLE encrypted_secrets ADD CONSTRAINT encrypted_secrets_purpose_check
    CHECK (purpose IN ('connector_credential', 'oauth_token', 'ai_provider_key', 'webhook_secret',
                       'webhook_signing', 'destination_credential', 'dkim_key',
                       'workspace_credential'));

CREATE TABLE credentials (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- A short stable identifier, unique among the workspace's live credentials.
    key             text NOT NULL CHECK (key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    name            text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    -- The schema id: a built-in kind, `custom` (the schema below) or `imported` (a credential a
    -- source or destination sealed on its own before credentials existed: no schema, every field
    -- secret).
    kind            text NOT NULL CHECK (char_length(kind) BETWEEN 1 AND 64),
    schema_version  integer NOT NULL DEFAULT 1 CHECK (schema_version >= 1),
    -- The schema of a `custom` credential; null for the other kinds.
    schema          jsonb,
    -- The non-secret values only (an account id, a host, a port, a region).
    fields          jsonb NOT NULL DEFAULT '{}',
    -- Every secret field as one sealed JSON object. The associated data binds the workspace, the
    -- secret id and the purpose, so a credential imported from an older secret keeps that
    -- secret's purpose until its next rotation re-seals it as `workspace_credential`.
    secret_id       uuid NOT NULL,
    -- Per secret field: its last four characters when the value has at least 16, else ''.
    hints           jsonb NOT NULL DEFAULT '{}',
    -- The redaction HMAC of the sealed object; null for an imported credential until it rotates,
    -- and when the process had no HMAC root.
    fingerprint     text,
    rotated_at      timestamptz NOT NULL DEFAULT now(),
    rotated_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    last_tested_at  timestamptz,
    last_test_outcome text CHECK (last_test_outcome IN ('ok', 'failed')),
    archived_at     timestamptz,
    created_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- The composite key refuses a secret of another workspace.
    FOREIGN KEY (workspace_id, secret_id) REFERENCES encrypted_secrets (workspace_id, id)
);

CREATE UNIQUE INDEX credentials_key_idx ON credentials (workspace_id, key)
    WHERE archived_at IS NULL;
CREATE UNIQUE INDEX credentials_name_idx ON credentials (workspace_id, lower(name))
    WHERE archived_at IS NULL;
CREATE UNIQUE INDEX credentials_secret_idx ON credentials (workspace_id, secret_id);
-- Archived credentials sort apart; the list reads the workspace's live ones by name.
CREATE INDEX credentials_list_idx ON credentials (workspace_id, archived_at, lower(name));

-- A source or destination references a credential instead of sealing a secret of its own.
ALTER TABLE sources ADD COLUMN credential_id uuid;
ALTER TABLE destinations ADD COLUMN credential_id uuid;

-- Existing sealed credentials become credentials that point at the same `encrypted_secrets` row.
-- Nothing is re-sealed here: the ciphertext keeps its row and purpose, which the associated data
-- binds. The id of the owner in the key and the name keeps every generated value unique.
--
-- The MySQL and PostgreSQL sources named their user field `user` and now name it `username`, as the
-- built-in "Username and password" kind does, so a credential can be shared with them. The
-- imported credential is named for the new field; the sealed JSON still holds `user`, which the
-- sources read as a fallback, and the next rotation seals `username`.
CREATE TEMPORARY TABLE imported_credentials AS
    SELECT 'source'::text AS owner, s.id AS owner_id, gen_random_uuid() AS id, s.workspace_id,
           s.name AS owner_name, s.credential_secret_id AS secret_id,
           CASE WHEN s.kind IN ('mysql', 'postgres')
                THEN array_replace(s.credential_fields, 'user', 'username')
                ELSE s.credential_fields END AS credential_fields,
           s.created_by
    FROM sources s
    WHERE s.credential_secret_id IS NOT NULL
    UNION ALL
    SELECT 'destination', d.id, gen_random_uuid(), d.workspace_id, d.name, d.credential_secret_id,
           d.credential_fields, d.created_by
    FROM destinations d
    WHERE d.credential_secret_id IS NOT NULL;

INSERT INTO credentials (id, workspace_id, key, name, kind, secret_id, hints, created_by)
SELECT i.id, i.workspace_id, i.owner || '-' || replace(i.owner_id::text, '-', ''),
       left(i.owner_name, 80) || ' (' || i.owner || ' ' || replace(i.owner_id::text, '-', '') || ')',
       'imported', i.secret_id,
       coalesce((SELECT jsonb_object_agg(f, '') FROM unnest(i.credential_fields) AS f), '{}'),
       i.created_by
FROM imported_credentials i;

UPDATE sources s SET credential_id = i.id
FROM imported_credentials i WHERE i.owner = 'source' AND i.owner_id = s.id;
UPDATE destinations d SET credential_id = i.id
FROM imported_credentials i WHERE i.owner = 'destination' AND i.owner_id = d.id;

DROP TABLE imported_credentials;

ALTER TABLE sources
    ADD CONSTRAINT sources_credential_fk FOREIGN KEY (workspace_id, credential_id)
        REFERENCES credentials (workspace_id, id);
ALTER TABLE destinations
    ADD CONSTRAINT destinations_credential_fk FOREIGN KEY (workspace_id, credential_id)
        REFERENCES credentials (workspace_id, id);
CREATE INDEX sources_credential_idx ON sources (workspace_id, credential_id)
    WHERE credential_id IS NOT NULL;
CREATE INDEX destinations_credential_idx ON destinations (workspace_id, credential_id)
    WHERE credential_id IS NOT NULL;

-- The old columns are gone: the secret and the field names now come from the credential, through
-- these two functions, so a rotation needs no change to any source or destination.
ALTER TABLE sources DROP COLUMN credential_secret_id, DROP COLUMN credential_fields;
ALTER TABLE destinations DROP COLUMN credential_secret_id, DROP COLUMN credential_fields;

CREATE FUNCTION credential_secret_of(ws uuid, credential uuid) RETURNS uuid
LANGUAGE sql STABLE AS $$
    SELECT c.secret_id FROM credentials c WHERE c.workspace_id = ws AND c.id = credential
$$;

-- The names of the fields a credential holds: its non-secret fields and its secret fields.
CREATE FUNCTION credential_field_names(ws uuid, credential uuid) RETURNS text[]
LANGUAGE sql STABLE AS $$
    SELECT coalesce(array_agg(k ORDER BY k), '{}'::text[])
    FROM (SELECT jsonb_object_keys(c.fields) AS k FROM credentials c
          WHERE c.workspace_id = ws AND c.id = credential
          UNION
          SELECT jsonb_object_keys(c.hints) FROM credentials c
          WHERE c.workspace_id = ws AND c.id = credential) names
$$;
