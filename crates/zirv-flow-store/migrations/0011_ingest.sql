-- The durable ingestion inbox (architecture sections 4.4 and 7.1): an append is the
-- acknowledgement, and `ingest.drain` applies and deletes rows in one transaction.

CREATE TABLE ingest_inbox (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    api_key_id   uuid NOT NULL,
    kind         text NOT NULL CHECK (kind IN ('identify', 'track')),
    -- The raw item until it is drained; never logged.
    item         jsonb NOT NULL,
    message_id   text NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    trace_id     uuid NOT NULL
);

-- The drain's ordered scan and the leader's oldest-row probe.
CREATE INDEX ingest_inbox_workspace_idx ON ingest_inbox (workspace_id, id);

-- Rows churn constantly (insert, drain, delete), so vacuum early and often.
ALTER TABLE ingest_inbox SET (
    autovacuum_vacuum_scale_factor = 0.01,
    autovacuum_vacuum_threshold = 100,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 2
);
