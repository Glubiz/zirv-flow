-- Sources and their streams (architecture sections 5.3 and 7.2). A source is a configured
-- external system; its credential lives only in encrypted_secrets, referenced by id. A stream
-- is one thing a source offers, with the user's sync settings. Neither table holds a person.

-- A source may only reference a secret of its own workspace: the composite keys below make the
-- database refuse a cross-workspace link.
ALTER TABLE encrypted_secrets ADD CONSTRAINT encrypted_secrets_workspace_id_key UNIQUE (workspace_id, id);

CREATE TABLE sources (
    id                   uuid PRIMARY KEY,
    workspace_id         uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    kind                 text NOT NULL CHECK (char_length(kind) BETWEEN 1 AND 64),
    name                 text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    config               jsonb NOT NULL,
    -- Deleting a source deletes this row first, then the secret, in one transaction.
    credential_secret_id uuid,
    -- The names of the credential fields that are set; never their values.
    credential_fields    text[] NOT NULL DEFAULT '{}',
    schedule             text,
    enabled              boolean NOT NULL DEFAULT true,
    created_by           uuid REFERENCES users (id),
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name),
    UNIQUE (workspace_id, id),
    -- MATCH SIMPLE: a NULL secret id is allowed.
    FOREIGN KEY (workspace_id, credential_secret_id) REFERENCES encrypted_secrets (workspace_id, id)
);

CREATE INDEX sources_credential_secret_idx ON sources (credential_secret_id);

CREATE TABLE streams (
    id                          uuid PRIMARY KEY,
    workspace_id                uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    source_id                   uuid NOT NULL,
    name                        text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 256),
    enabled                     boolean NOT NULL DEFAULT false,
    sync_mode                   text NOT NULL CHECK (sync_mode IN ('full_refresh', 'incremental')),
    cursor_field                text,
    primary_key                 text[] NOT NULL DEFAULT '{}',
    -- Field mapping; set by the sync issues, null until then.
    mapping                     jsonb,
    staleness_threshold_seconds integer CHECK (staleness_threshold_seconds > 0),
    -- The last descriptor discovery reported for this stream.
    discovered                  jsonb NOT NULL,
    created_at                  timestamptz NOT NULL DEFAULT now(),
    updated_at                  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (source_id, name),
    FOREIGN KEY (workspace_id, source_id) REFERENCES sources (workspace_id, id) ON DELETE CASCADE,
    CHECK (sync_mode <> 'incremental' OR cursor_field IS NOT NULL)
);

CREATE INDEX streams_workspace_idx ON streams (workspace_id, source_id);
