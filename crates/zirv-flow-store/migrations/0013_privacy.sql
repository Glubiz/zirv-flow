-- Deletion, erasure and export (architecture sections 7.4 and 16.3): the workspace orphan
-- policy, the erasure ledger and the person exports.

-- What happens to a person who has no contribution, event or explicit consent left after a
-- source retracts a record: keep them, or delete them.
ALTER TABLE workspaces
    ADD COLUMN orphan_policy text NOT NULL DEFAULT 'keep'
        CHECK (orphan_policy IN ('keep', 'delete'));

-- The erasure ledger. It is the one table that intentionally outlives an erased person: it
-- keeps the person id and the keyed hashes of their identifiers, never a value, so a restore
-- from a backup can be erased again. person_id carries no foreign key for that reason, and
-- the erasure scan skips this table by name.
CREATE TABLE erasure_requests (
    id                uuid PRIMARY KEY,
    workspace_id      uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id         uuid NOT NULL,
    -- HMAC-SHA256 of every identity of the person and of the people merged into them, and
    -- the kind of each, in the same order.
    identifier_hashes bytea[] NOT NULL DEFAULT '{}',
    identifier_kinds  text[] NOT NULL DEFAULT '{}',
    -- The ids of the person and of the people merged into them when the erasure ran, so a
    -- restore from before a merge can erase each of them.
    person_ids        uuid[] NOT NULL DEFAULT '{}',
    status            text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'completed', 'failed')),
    requested_by      uuid,
    requested_at      timestamptz NOT NULL DEFAULT now(),
    completed_at      timestamptz,
    trace_id          uuid NOT NULL,
    CHECK (cardinality(identifier_hashes) = cardinality(identifier_kinds))
);

-- Newest first, for the request list.
CREATE INDEX erasure_requests_listing_idx
    ON erasure_requests (workspace_id, requested_at DESC, id DESC);

-- A person export. The content is gzip-compressed JSON and is dropped when the export
-- expires. person_id names the person, so the erasure scan finds these rows and erasure
-- deletes them.
CREATE TABLE privacy_exports (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id    uuid NOT NULL REFERENCES people (id),
    status       text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'ready', 'failed', 'expired')),
    requested_by uuid,
    requested_at timestamptz NOT NULL DEFAULT now(),
    ready_at     timestamptz,
    -- ready_at plus 24 hours.
    expires_at   timestamptz,
    content      bytea,
    size_bytes   bigint,
    trace_id     uuid NOT NULL
);

-- Newest first, for the request list.
CREATE INDEX privacy_exports_listing_idx
    ON privacy_exports (workspace_id, requested_at DESC, id DESC);
CREATE INDEX privacy_exports_person_idx ON privacy_exports (workspace_id, person_id);
-- The hourly upkeep finds the exports to expire here.
CREATE INDEX privacy_exports_expiry_idx ON privacy_exports (expires_at) WHERE status = 'ready';

-- The request lists show a request whose job went dead as failed; they find the job by its
-- unique key, which a dead job keeps but no longer holds in the unique index.
CREATE INDEX jobs_dead_unique_key_idx ON jobs (unique_key)
    WHERE state = 'dead' AND unique_key IS NOT NULL;
