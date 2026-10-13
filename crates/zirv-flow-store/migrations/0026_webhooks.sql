-- Inbound webhooks (architecture sections 7.1 and 16.1): a hook is a source of kind `webhook`
-- with one stream `payload`, plus a `webhooks` row holding what the receive path needs. The
-- shared secret is a sealed `encrypted_secrets` row; nothing here holds a payload value except
-- the transient inbox row, which the drain deletes.

ALTER TABLE encrypted_secrets DROP CONSTRAINT encrypted_secrets_purpose_check;
ALTER TABLE encrypted_secrets ADD CONSTRAINT encrypted_secrets_purpose_check
    CHECK (purpose IN ('connector_credential', 'oauth_token', 'ai_provider_key', 'webhook_secret'));

CREATE TABLE webhooks (
    -- A random v4 UUID: the id in the URL. It locates the hook; the HMAC authorizes.
    id                uuid PRIMARY KEY,
    workspace_id      uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    source_id         uuid NOT NULL,
    stream_id         uuid NOT NULL,
    secret_id         uuid NOT NULL,
    signature_header  text NOT NULL CHECK (char_length(signature_header) BETWEEN 1 AND 64),
    signature_prefix  text NOT NULL DEFAULT 'sha256=' CHECK (char_length(signature_prefix) <= 32),
    -- NULL selects the body-only scheme.
    timestamp_header  text CHECK (timestamp_header IS NULL OR char_length(timestamp_header) BETWEEN 1 AND 64),
    tolerance_seconds integer NOT NULL DEFAULT 300 CHECK (tolerance_seconds BETWEEN 1 AND 86400),
    dedupe_field      text NOT NULL CHECK (char_length(dedupe_field) BETWEEN 1 AND 256),
    created_by        uuid REFERENCES users (id),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (source_id),
    FOREIGN KEY (workspace_id, source_id) REFERENCES sources (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, stream_id) REFERENCES streams (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, secret_id) REFERENCES encrypted_secrets (workspace_id, id)
);
CREATE INDEX webhooks_workspace_idx ON webhooks (workspace_id);
CREATE INDEX webhooks_secret_idx ON webhooks (secret_id);

-- A webhook row has a hook instead of an API key.
ALTER TABLE ingest_inbox ALTER COLUMN api_key_id DROP NOT NULL;
ALTER TABLE ingest_inbox ADD COLUMN webhook_id uuid REFERENCES webhooks (id) ON DELETE CASCADE;
ALTER TABLE ingest_inbox DROP CONSTRAINT ingest_inbox_kind_check;
ALTER TABLE ingest_inbox ADD CONSTRAINT ingest_inbox_kind_check
    CHECK (kind IN ('identify', 'track', 'contact', 'webhook'));
ALTER TABLE ingest_inbox ADD CONSTRAINT ingest_inbox_origin_check CHECK (
    (kind = 'webhook' AND webhook_id IS NOT NULL AND api_key_id IS NULL)
 OR (kind <> 'webhook' AND api_key_id IS NOT NULL AND webhook_id IS NULL));
CREATE INDEX ingest_inbox_webhook_idx ON ingest_inbox (webhook_id) WHERE webhook_id IS NOT NULL;
