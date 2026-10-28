-- What an Idempotency-Key was used for on the creating requests that no unique name or draft
-- revision already guards (architecture section 15). A request first reserves its key as
-- `pending`, with a lease; the create marks it `done` in its own transaction, so a create commits
-- only while it owns the key. The key is stored as the hash of the caller and the key, the
-- request and response as hashes, and the created resource by id, so the table holds no person
-- column, no client-chosen text and no response body. A pending row whose lease has run out may be taken
-- over; rows expire after 24 hours, in the leader's upkeep.
CREATE TABLE api_idempotency (
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    route         text NOT NULL,
    key_hash      bytea NOT NULL,
    state         text NOT NULL CHECK (state IN ('pending', 'done')),
    reservation   uuid NOT NULL,
    lease_until   timestamptz NOT NULL,
    -- NULL while an upload that streams its body has not finished hashing it.
    request_hash  bytea,
    response_hash bytea,
    status        smallint,
    resource_id   uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, route, key_hash),
    CHECK (state = 'pending' OR (status IS NOT NULL AND resource_id IS NOT NULL
        AND request_hash IS NOT NULL AND response_hash IS NOT NULL))
);

CREATE INDEX api_idempotency_created_idx ON api_idempotency (created_at);
