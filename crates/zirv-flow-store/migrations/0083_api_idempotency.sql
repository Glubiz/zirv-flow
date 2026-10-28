-- What an Idempotency-Key was used for on the creating requests that no unique name or draft
-- revision already guards (architecture section 15). The key is stored as its hash, the request
-- as its hash, and the created resource by id, so the table holds no person column and no
-- client-chosen text. A response body is kept only for a route whose response holds neither a
-- secret nor a person's value; the other routes answer a replay with the id alone. Rows expire
-- after 24 hours, in the leader's upkeep.
CREATE TABLE api_idempotency (
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    route        text NOT NULL,
    key_hash     bytea NOT NULL,
    request_hash bytea NOT NULL,
    status       smallint NOT NULL,
    resource_id  uuid NOT NULL,
    response     jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, route, key_hash)
);

CREATE INDEX api_idempotency_created_idx ON api_idempotency (created_at);
