-- Native contacts (architecture section 7.6): the per-workspace contact definition, the
-- idempotency keys of the synchronous contacts API, and a `contact` kind for the ingestion
-- inbox. A contact itself is a live `source_records` row (source `native`, stream `contacts`,
-- record_key = the contact key); it needs no table of its own.

CREATE TABLE contact_definitions (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    -- [{key, required, identifier}]: each key names a field-registry key.
    fields       jsonb NOT NULL DEFAULT '[]'::jsonb,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   uuid
);

-- What an Idempotency-Key was used for. It holds no field values and no person id: a replay
-- answers with the contact as it is stored now. `contact_key` is the identifier value
-- `external:contact_key`, so a person's deletion clears the rows for the contact keys it held.
-- Rows expire after 24 hours, in the leader's upkeep.
CREATE TABLE contact_idempotency (
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    key          text NOT NULL,
    request_hash bytea NOT NULL,
    status       smallint NOT NULL,
    contact_key  text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, key)
);

CREATE INDEX contact_idempotency_contact_idx ON contact_idempotency (workspace_id, contact_key);
CREATE INDEX contact_idempotency_created_idx ON contact_idempotency (created_at);

ALTER TABLE ingest_inbox DROP CONSTRAINT ingest_inbox_kind_check;
ALTER TABLE ingest_inbox
    ADD CONSTRAINT ingest_inbox_kind_check CHECK (kind IN ('identify', 'track', 'contact'));
