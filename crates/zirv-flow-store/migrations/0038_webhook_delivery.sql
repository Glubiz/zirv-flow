-- Signed webhook delivery (architecture 9.1 and 16.2). A workspace signs the calls of its
-- `action.webhook` nodes with one secret, a sealed `encrypted_secrets` row of purpose
-- `webhook_signing`. This table says which of those rows is the current secret and which one a
-- rotation retired: the retired secret still signs for a grace window, and a rotation deletes
-- whichever retired secret was still waiting (the cascade removes its key row with it).
CREATE TABLE webhook_signing_keys (
    secret_id    uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL,
    -- Null for the current secret; the time of the rotation that replaced it.
    retired_at   timestamptz,
    FOREIGN KEY (workspace_id, secret_id)
        REFERENCES encrypted_secrets (workspace_id, id) ON DELETE CASCADE
);

-- One current secret per workspace.
CREATE UNIQUE INDEX webhook_signing_keys_current_idx
    ON webhook_signing_keys (workspace_id) WHERE retired_at IS NULL;
CREATE INDEX webhook_signing_keys_workspace_idx ON webhook_signing_keys (workspace_id);
