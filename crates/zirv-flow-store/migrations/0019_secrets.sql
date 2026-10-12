-- Envelope-encrypted secrets (architecture section 16.2). A secret is encrypted with its own
-- random data key; the data key is wrapped by a key encryption key derived from the operator's
-- master key. The database holds ciphertext, the wrapped key and the id of the wrapping
-- master key, never a plaintext or a master key. Re-wrapping on a master key rotation changes
-- key_id and wrapped_key only.

CREATE TABLE encrypted_secrets (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    purpose      text NOT NULL CHECK (purpose IN ('connector_credential', 'oauth_token', 'ai_provider_key')),
    key_id       text NOT NULL,
    -- A 24-byte nonce followed by the wrapped data key and its tag.
    wrapped_key  bytea NOT NULL,
    nonce        bytea NOT NULL CHECK (length(nonce) = 24),
    ciphertext   bytea NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX encrypted_secrets_workspace_idx ON encrypted_secrets (workspace_id);
-- Rotation finds the rows a master key still wraps.
CREATE INDEX encrypted_secrets_key_id_idx ON encrypted_secrets (key_id);

-- Keys that belong to the deployment rather than a workspace, sealed the same way. The only
-- one is the root every HMAC key derives from, which stays fixed across master key rotations.
CREATE TABLE deployment_keys (
    name        text PRIMARY KEY CHECK (name IN ('hmac_root')),
    key_id      text NOT NULL,
    wrapped_key bytea NOT NULL,
    nonce       bytea NOT NULL CHECK (length(nonce) = 24),
    ciphertext  bytea NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX deployment_keys_key_id_idx ON deployment_keys (key_id);
