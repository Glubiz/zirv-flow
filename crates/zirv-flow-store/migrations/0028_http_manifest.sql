-- The HTTP manifest source (architecture sections 7.2 and 7.5): the OAuth2 authorization-code
-- consent in progress, and the link from a source to its token secret. Nothing here holds a
-- record value, a manifest (it is `sources.config`) or a plaintext token.

-- The sealed access and refresh tokens of a source live in their own `encrypted_secrets` row
-- (purpose `oauth_token`), apart from the credential row: a sync job refreshes tokens while an
-- admin may be replacing the credential, and one row would lose one of the two writes.
ALTER TABLE sources
    ADD COLUMN oauth_secret_id uuid,
    ADD COLUMN oauth_connected_at timestamptz,
    ADD CONSTRAINT sources_oauth_secret_fk FOREIGN KEY (workspace_id, oauth_secret_id)
        REFERENCES encrypted_secrets (workspace_id, id);
CREATE INDEX sources_oauth_secret_idx ON sources (oauth_secret_id)
    WHERE oauth_secret_id IS NOT NULL;

-- A consent in progress: the hash of the single-use `state`, the PKCE verifier, the token binding of
-- the manifest it was started under (a hash, never a secret), and who started it for which source. Taken (deleted) by the callback, so it can be used once; expires after
-- ten minutes. Deleting the source or the user deletes it.
CREATE TABLE source_oauth_states (
    state_hash   bytea PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    source_id    uuid NOT NULL,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    pkce_verifier text NOT NULL,
    binding      text NOT NULL,
    expires_at   timestamptz NOT NULL,
    FOREIGN KEY (workspace_id, source_id) REFERENCES sources (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX source_oauth_states_expires_idx ON source_oauth_states (expires_at);
