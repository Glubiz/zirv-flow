-- Workspace invitations (#605). An Admin invites an address into a workspace with a role and a
-- personal-data grant; the invitee accepts through a one-time link and becomes a member.
--
-- `token_hash` is the SHA-256 of the 32 random bytes the link carries; the token itself is never
-- stored. `email` is the invitee's address as the Admin typed it, kept so the accept step can
-- compare it with the signed-in user's address under `lower()`, the same rule as `users_email_key`.
-- It is a staff invitee's address, like `users.email`, not a workspace person's, so the person
-- erasure path does not touch it. `expires_at` is checked at read time: there is no sweep job.
--
-- INDEX: `invitations_live_email_key` holds one live invitation per workspace and lower(email);
-- `now()` cannot appear in an index predicate, so an expired but unrevoked row keeps its slot until
-- the next invitation for that address revokes it in its own transaction.
-- `invitations_workspace_id_idx` serves the pending list of one workspace, newest first. The
-- `token_hash` UNIQUE constraint is the lookup index of the link.
--
-- UPGRADE COST: a new table, a unique constraint and two indexes on it; the foreign keys take a
-- brief lock on `workspaces` and `users`, which are not scanned. No existing row is rewritten.
-- This is a new table of low volume (one row per invitation), not a hot table, so the write-cost
-- rule for the hot tables does not apply to it.
CREATE TABLE invitations (
    id           uuid PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    email        text        NOT NULL,
    role         text        NOT NULL CHECK (role IN ('admin', 'publisher', 'editor', 'viewer')),
    view_pii     boolean     NOT NULL DEFAULT false,
    token_hash   bytea       NOT NULL UNIQUE,
    invited_by   uuid        REFERENCES users (id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    accepted_at  timestamptz,
    accepted_by  uuid        REFERENCES users (id) ON DELETE SET NULL,
    revoked_at   timestamptz
);
CREATE UNIQUE INDEX invitations_live_email_key
    ON invitations (workspace_id, lower(email))
    WHERE accepted_at IS NULL AND revoked_at IS NULL;
CREATE INDEX invitations_workspace_id_idx ON invitations (workspace_id, created_at DESC);
