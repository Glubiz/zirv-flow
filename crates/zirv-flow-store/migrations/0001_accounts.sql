-- Local accounts, workspaces, sessions, API keys and the audit log.

CREATE TABLE users (
    id              uuid PRIMARY KEY,
    email           text NOT NULL,
    password_hash   text NOT NULL,
    is_owner        boolean NOT NULL DEFAULT false,
    failed_sign_ins integer NOT NULL DEFAULT 0,
    locked_until    timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX users_email_key ON users (lower(email));
-- At most one instance owner, enforced by the database so two concurrent
-- `create-owner` runs cannot both succeed.
CREATE UNIQUE INDEX users_single_owner ON users ((true)) WHERE is_owner;

CREATE TABLE workspaces (
    id         uuid PRIMARY KEY,
    name       text NOT NULL,
    slug       text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE memberships (
    workspace_id uuid NOT NULL REFERENCES workspaces (id),
    user_id      uuid NOT NULL REFERENCES users (id),
    role         text NOT NULL CHECK (role IN ('admin', 'member')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);

CREATE INDEX memberships_user_id_idx ON memberships (user_id);

CREATE TABLE sessions (
    token_hash   bytea PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    csrf_hash    bytea NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL
);

CREATE INDEX sessions_user_id_idx ON sessions (user_id);

CREATE TABLE api_keys (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id),
    name         text NOT NULL,
    secret_hash  bytea NOT NULL,
    scopes       text[] NOT NULL,
    created_by   uuid REFERENCES users (id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);

CREATE INDEX api_keys_workspace_id_idx ON api_keys (workspace_id);

-- Append-only: the store exposes no update or delete for this table. It holds
-- no personal data values (no emails, passwords, tokens or key secrets).
CREATE TABLE audit_log (
    id           bigserial PRIMARY KEY,
    workspace_id uuid,
    actor_kind   text NOT NULL CHECK (actor_kind IN ('user', 'api_key', 'system', 'anonymous')),
    actor_id     uuid,
    action       text NOT NULL,
    object_kind  text,
    object_id    uuid,
    request_id   text,
    summary      jsonb NOT NULL DEFAULT '{}',
    occurred_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_log_workspace_id_idx ON audit_log (workspace_id, id);
