-- Destinations (architecture 5.3, 10.1 and 14): the configured external systems messages are
-- delivered to, the cross-worker rate bucket (ruling OQ-9) and the record of each delivery
-- attempt. A destination's credential lives only in encrypted_secrets, referenced by id.
--
-- None of these tables is person-scoped. `delivery_attempts` names the message, forwarding or sync
-- item an attempt was for and never a person or an address, so the erasure scan finds no person
-- column here.

-- A destination's credential is a sealed secret of its own purpose.
ALTER TABLE encrypted_secrets DROP CONSTRAINT encrypted_secrets_purpose_check;
ALTER TABLE encrypted_secrets ADD CONSTRAINT encrypted_secrets_purpose_check
    CHECK (purpose IN ('connector_credential', 'oauth_token', 'ai_provider_key', 'webhook_secret',
                       'webhook_signing', 'destination_credential'));

CREATE TABLE destinations (
    id                   uuid PRIMARY KEY,
    workspace_id         uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    kind                 text NOT NULL CHECK (char_length(kind) BETWEEN 1 AND 64),
    name                 text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    config               jsonb NOT NULL,
    credential_secret_id uuid,
    -- The names of the credential fields that are set; never their values.
    credential_fields    text[] NOT NULL DEFAULT '{}',
    -- The rate the destination tolerates, in deliveries per second, and the most it may send at
    -- once. The bucket's own rate drops below this while a 429 holds it back.
    rate                 integer NOT NULL DEFAULT 10 CHECK (rate BETWEEN 1 AND 100000),
    burst                integer NOT NULL DEFAULT 10 CHECK (burst BETWEEN 1 AND 100000),
    archived_at          timestamptz,
    created_by           uuid REFERENCES users (id),
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- MATCH SIMPLE: a NULL secret id is allowed. The composite key refuses a cross-workspace link.
    FOREIGN KEY (workspace_id, credential_secret_id) REFERENCES encrypted_secrets (workspace_id, id)
);

-- A live destination's name is unique in its workspace, ignoring case; an archived name is free.
CREATE UNIQUE INDEX destinations_name_idx ON destinations (workspace_id, lower(name))
    WHERE archived_at IS NULL;
CREATE INDEX destinations_credential_secret_idx ON destinations (credential_secret_id);

-- One row per destination, shared by every worker of every process. A worker takes tokens with
-- one UPDATE that refills from `refilled_at` on the database's clock and grants in the same
-- statement, so concurrent workers never double-grant.
CREATE TABLE destination_rate_buckets (
    destination_id uuid PRIMARY KEY,
    workspace_id   uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The effective refill rate, tokens per second. Below the destination's rate while a 429
    -- penalty or its recovery lasts.
    rate           double precision NOT NULL CHECK (rate >= 1),
    burst          integer NOT NULL CHECK (burst >= 1),
    -- Fractional tokens, never above `burst`.
    tokens         double precision NOT NULL CHECK (tokens >= 0),
    refilled_at    timestamptz NOT NULL,
    -- Set by a 429: the rate stays halved until then, then recovers additively.
    penalty_until  timestamptz,
    FOREIGN KEY (workspace_id, destination_id) REFERENCES destinations (workspace_id, id)
        ON DELETE CASCADE
);

-- The rate a bucket refills at, at the instant `ts`. With no penalty it is the destination's own
-- rate. A 429 stores the halved rate and `penalty_until`: the rate stays there until then, and
-- afterwards recovers additively, by a tenth of the destination's rate (at least 1) per second,
-- up to the destination's rate. The stored rate is left alone during recovery, so this is a pure
-- function of the row and the instant.
CREATE FUNCTION destination_bucket_rate(stored double precision, cap double precision,
                                        penalty_until timestamptz, ts timestamptz)
RETURNS double precision LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN penalty_until IS NULL THEN cap
        WHEN ts <= penalty_until THEN LEAST(stored, cap)
        ELSE LEAST(cap, LEAST(stored, cap)
                        + GREATEST(1, CEIL(cap * 0.1)) * EXTRACT(EPOCH FROM (ts - penalty_until)))
    END
$$;

-- What each delivery attempt did. The subject is the item delivered: a message, a forwarding
-- item or a sync item. No person id and no address is stored here.
CREATE TABLE delivery_attempts (
    id             uuid PRIMARY KEY,
    workspace_id   uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    destination_id uuid NOT NULL,
    subject_kind   text NOT NULL CHECK (subject_kind IN ('message', 'forwarding', 'sync')),
    subject_id     uuid NOT NULL,
    attempt        integer NOT NULL CHECK (attempt >= 1),
    outcome        text NOT NULL CHECK (outcome IN ('delivered', 'retryable', 'terminal', 'throttled')),
    -- The class of the answer's HTTP status; none when the call produced no answer.
    status_class   text NOT NULL CHECK (status_class IN ('none', '2xx', '3xx', '4xx', '5xx')),
    duration_ms    integer NOT NULL CHECK (duration_ms >= 0),
    attempted_at   timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, destination_id) REFERENCES destinations (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (destination_id, subject_kind, subject_id, attempt)
);

CREATE INDEX delivery_attempts_destination_idx ON delivery_attempts (destination_id, attempted_at);
