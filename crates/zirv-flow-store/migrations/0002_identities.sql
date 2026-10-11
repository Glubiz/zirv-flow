-- OIDC sign-in: links between users and external identities, and the
-- short-lived state of logins in progress.

-- A user who signs in only through an identity provider has no password.
ALTER TABLE users ALTER COLUMN password_hash DROP NOT NULL;

CREATE TABLE user_identities (
    issuer     text NOT NULL,
    subject    text NOT NULL,
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issuer, subject),
    UNIQUE (issuer, user_id)
);

-- One row per login in progress, keyed by the hash of `state`. Single-use and
-- short-lived; holds no personal data.
CREATE TABLE oidc_logins (
    state_hash    bytea PRIMARY KEY,
    nonce         text NOT NULL,
    pkce_verifier text NOT NULL,
    expires_at    timestamptz NOT NULL
);
