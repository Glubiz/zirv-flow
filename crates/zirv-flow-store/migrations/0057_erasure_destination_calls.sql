-- The erase call made to each destination when a person is erased (architecture 16.3, "As built
-- (erase calls, #227)"). One row per erasure request and destination, written `pending` in the
-- erasure transaction and settled by the `destination.erase` job.
--
-- The table is keyed to the erasure ledger, not to a person: it holds no person id, no address and
-- no key sent to the destination, so the erasure scan finds nothing here. `error_code` is a fixed
-- code (a verdict reason), never a message, a URL or a value.

CREATE TABLE erasure_destination_calls (
    erasure_id     uuid NOT NULL REFERENCES erasure_requests (id) ON DELETE CASCADE,
    workspace_id   uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    destination_id uuid NOT NULL,
    outcome        text NOT NULL DEFAULT 'pending' CHECK (outcome IN ('pending', 'done', 'failed')),
    -- Calls the job made; zero while the call is pending.
    attempts       integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    error_code     text CHECK (error_code ~ '^[a-z0-9_]{1,64}$'),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (erasure_id, destination_id),
    -- A failure has a code; the other outcomes have none.
    CHECK ((outcome = 'failed') = (error_code IS NOT NULL)),
    FOREIGN KEY (workspace_id, destination_id) REFERENCES destinations (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX erasure_destination_calls_workspace_idx
    ON erasure_destination_calls (workspace_id, erasure_id);
